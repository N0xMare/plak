//! A retained predecessor job must not enter the successor's foreground cleanup.

use super::*;
use plakous::sessions::AgentSessions;
use plakous::sessions::feed::{
    FeedCursor, FeedFact, FeedPage, FeedPageRequest, FeedRecord, FeedTurnOutcome,
};
use plakous_recursive_subagents::protocol::{AttemptStatus, MessagePurpose};

#[test]
fn finite_exec_drain_does_not_stop_an_acknowledged_predecessor_job() {
    let Some(fixture) = fixture() else { return };
    let sut = FiniteDrain::new(fixture);
    let probes = sut.fixture.root.join("job-probes");
    std::fs::create_dir(&probes).unwrap();
    let (mut host, child) = child_with_pending_root(
        &sut,
        Some(("PLAK_TEST_RUNNER_JOB_PROBE", probes.display().to_string())),
    );
    let root = shell_words::quote(sut.fixture.root.to_str().unwrap());
    let command = format!(
        "base={root}/successor-fg; printf '%s %s\\n' \"$$\" \"$PPID\" >> \"$base.marker\"; \
         n=0; while test ! -f \"$base.release\"; do [ \"$n\" -lt 1200 ] || exit 1; \
         n=$((n + 1)); sleep .05; done"
    );
    let submit = sut.fixture.root.join("submit-child");
    let output = r#"{"summary":"review complete"}"#;
    std::fs::write(
        sut.fixture.root.join("child.json"),
        json!({"version": 1, "process_tools": true, "steps": [
            {"gate": {"file": sut.fixture.root.join("resume-release")},
             "output": {"tool": {"name": "bash", "arguments": {"command": command, "timeout": 600}}},
             "finish": "tool_calls"},
            {"gate": {"file": submit}, "output": {"tool": {"name": "submit_result", "arguments": {
                "turn_token": 1, "output_json": output
            }}}, "finish": "tool_calls"},
            {"output": {"text": "review complete"}}
        ]})
        .to_string(),
    )
    .unwrap();
    sut.upgrade(&mut host, "0");
    release_continuation(&sut.fixture);
    let (successor, successor_runner) = marked(&sut.fixture, "successor", "fg");
    assert_ne!(successor_runner, child.original.1);
    sut.finish_job(child.original);
    wait_until("the terminal job was not projected during Bash", || {
        probes.join("terminal-projection-1").is_file()
    });
    assert!(runner::alive(successor));
    std::fs::write(sut.fixture.root.join("successor-fg.release"), "").unwrap();
    wait_until(
        "ACK did not remove the retained job from the mirror",
        || probes.join("ack-1").is_file(),
    );

    let paths = plakd::home::paths::PlakPaths::explicit(sut.fixture.home());
    let store = SqliteTreeStore::open(&paths.subagents_database()).unwrap();
    let read = || ProductAgentTree::reopen(store.load(&child.parent.tree_id).unwrap().unwrap());
    let locator = child_locator(&store, &child);
    let tree = read();
    assert_eq!(loss_resumptions(&tree), 1);
    let command_id = tree
        .commands()
        .keys()
        .find(|id| id.starts_with(ProductAgentTree::LOSS_RESUME_COMMAND_PREFIX))
        .unwrap();
    let agent = &tree
        .nodes()
        .values()
        .find(|node| node.parent_agent_id.is_some())
        .unwrap()
        .agent_id;
    let database = paths.runtime_database();
    let terminals = || {
        let FeedPage::Records { records, .. } = AgentSessions::inspect_feed_sqlite(
            &database,
            FeedPageRequest::after(FeedCursor::start(locator.session_id())),
        )
        .unwrap() else {
            panic!("continued child feed should be retained");
        };
        records
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
            .collect::<Vec<_>>()
    };
    assert_eq!(retained_controls(&sut), 0, "ACK is not a stop");
    std::fs::write(submit, "").unwrap();
    wait_until("the continued runtime did not publish a terminal", || {
        !terminals().is_empty()
    });

    let controls = retained_controls(&sut);
    let observed = terminals();
    assert!(
        matches!(observed.as_slice(), [FeedTurnOutcome::Completed { .. }]) && controls == 0,
        "expected one completed runtime terminal and no retained stop, got {observed:?} and {controls} unacknowledged controls"
    );
    wait_until("the child result did not settle", || {
        read().attempt(agent).unwrap().status == AttemptStatus::Completed
    });
    stop(&mut host);
    assert_eq!(terminals(), observed);
    let tree = read();
    let results: Vec<_> = tree
        .messages()
        .values()
        .filter(|row| row.from == *agent && row.purpose == MessagePurpose::Result)
        .collect();
    assert_eq!(results.len(), 1);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&results[0].body).unwrap()["output"],
        output
    );
    assert_one_child_terminal(&database, child_locator(&store, &child));
}

fn wait_until(message: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !ready() {
        assert!(Instant::now() < deadline, "{message}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn retained_controls(sut: &FiniteDrain) -> usize {
    let builds = sut.paired.runner.home.join("runner/builds");
    let inventories: Vec<_> = std::fs::read_dir(builds)
        .unwrap()
        .map(|entry| entry.unwrap().path().join("retained-jobs.json"))
        .filter(|path| path.is_file())
        .collect();
    assert_eq!(inventories.len(), 1);
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(&inventories[0]).unwrap()).unwrap();
    let mut acknowledged = rows
        .iter()
        .filter(|row| !row["consumed"].as_array().unwrap().is_empty());
    let row = acknowledged.next().expect("no retained ACK receipt");
    assert!(acknowledged.next().is_none());
    assert_eq!(row["consumed"].as_array().unwrap().len(), 1);
    // The runner persists Sent before relaying the stop; the original worker
    // cannot join or acknowledge a control for the already-consumed job.
    row["controls"].as_array().unwrap().len()
}
