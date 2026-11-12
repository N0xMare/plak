//! Accepted queue facts and handoff decisions survive destination-side cuts.

use super::*;
use plak::store::session_records::{SessionRecordStore as _, SqliteSessionStore};
use plakous_offline_crash_harness::commit_trace::{KILL_ENV, TRACE_ENV};
use plakous_offline_crash_harness::evidence::{EvidenceKind, read_evidence};
use serde_json::Value;

fn store(fixture: &Fixture) -> SqliteSessionStore {
    SqliteSessionStore::new(plak::home::paths::PlakPaths::explicit(fixture.home()))
}

fn queue(fixture: &Fixture, session: &str) -> Vec<(String, String)> {
    store(fixture)
        .load(session)
        .unwrap()
        .pending_queue
        .into_iter()
        .map(|item| (item.id, item.text))
        .collect()
}

fn handed_off_seed() -> (Fixture, String) {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "steps": [{"stream": [{"text": "verdict-source-prefix"}, {"stall": 30000}]}]
    }))
    .with_session_lease_seconds(1);
    write_private(&fixture.root.join("quit-as-handoff"), "");
    let mut agent = AgentProcess::start(&fixture, &[]);
    let hello = agent.wait_for("hello", |row| row["kind"] == "hello");
    let session = hello["session_id"].as_str().unwrap().to_owned();
    agent.wait_for("ready", |row| row["kind"] == "ready");
    agent.send(&prompt_frame("original admitted turn"));
    agent.wait_for("streamed prefix", |row| {
        row["kind"] == "assistant"
            && row["text"]
                .as_str()
                .is_some_and(|text| text.contains("verdict-source-prefix"))
    });
    agent.send(&prompt_frame("accepted queue first"));
    agent.send(&prompt_frame("accepted queue second"));
    agent.wait_for("two durable queued inputs", |row| {
        row["kind"] == "queue"
            && row["items"]
                .as_array()
                .is_some_and(|items| items.len() == 2)
    });
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "{}", outcome.stderr);
    std::fs::remove_file(fixture.root.join("quit-as-handoff")).unwrap();
    assert_eq!(queue(&fixture, &session).len(), 2);
    assert_eq!(
        store(&fixture)
            .load_intents(&session)
            .unwrap()
            .root_interrupts
            .len(),
        1
    );
    (fixture, session)
}

fn dispatches(fixture: &Fixture) -> Vec<String> {
    let ids: Vec<_> = read_evidence(fixture.root.join("evidence.jsonl"))
        .unwrap()
        .into_iter()
        .filter(|row| row.kind == EvidenceKind::ProviderDispatched)
        .map(|row| row.operation_id.unwrap())
        .collect();
    let unique: std::collections::BTreeSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "no duplicate physical dispatch");
    ids
}

fn assert_queue_order(fixture: &Fixture, session: &str, accepted: &[(String, String)]) {
    // Recovery may also restore the host's continuation input. It must not
    // permute the separately accepted operator queue.
    let pending: Vec<_> = queue(fixture, session)
        .into_iter()
        .filter(|(id, _)| accepted.iter().any(|(accepted, _)| id == accepted))
        .collect();
    assert!(
        accepted.ends_with(&pending),
        "only the accepted FIFO prefix may drain: accepted={accepted:?}, pending={pending:?}"
    );
    let intents = store(fixture).load_intents(session).unwrap();
    let anchors: Vec<_> = accepted
        .iter()
        .filter_map(|(id, _)| {
            let submission = intents
                .submissions
                .iter()
                .find(|row| row.queue_item_id == *id)?;
            intents.submission_anchors.get(&submission.command_id)
        })
        .collect();
    assert!(
        anchors.windows(2).all(|pair| pair[0] <= pair[1]),
        "accepted order changed"
    );
}

fn finish_reopen(fixture: &Fixture, session: &str) {
    let mut agent = reopen_until_the_lease_clears(fixture, session);
    let standing = plakous_hosted::runtime::root_standing(&store(fixture), session).unwrap();
    if matches!(
        standing,
        plakous_hosted::runtime::RootStanding::Continuing { handoff: true, .. }
            | plakous_hosted::runtime::RootStanding::Continuing { held: false, .. }
    ) {
        agent.wait_for_completed_turns(1);
    }
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "{}", outcome.stderr);
}

fn kill_at(fixture: &Fixture, session: &str, target: &Value) {
    use std::os::unix::process::ExitStatusExt as _;
    let trace = fixture.root.join("cut-commits.jsonl");
    let mut command = fixture.command();
    command
        .args(["agent", "--session", session])
        .env(TRACE_ENV, &trace)
        .env(
            KILL_ENV,
            format!(
                "{}:{}",
                target["store"].as_str().unwrap(),
                target["store_ordinal"]
            ),
        );
    let mut agent = AgentProcess::start_command(command, true);
    let deadline = Instant::now() + WAIT;
    let mut quit = false;
    while agent.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "cut never fired: {target}");
        let completed = agent
            .snapshot()
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|row| row["kind"] == "assistant_done")
            .count();
        if !quit && completed == 3 {
            agent.send(&quit_frame());
            quit = true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        agent.finish().status.signal(),
        Some(9),
        "cut did not kill: {target}"
    );
    let last: Value = serde_json::from_str(
        std::fs::read_to_string(trace)
            .unwrap()
            .lines()
            .last()
            .unwrap(),
    )
    .unwrap();
    for field in ["store", "store_ordinal", "label", "tables", "classes"] {
        assert_eq!(last[field], target[field], "cut diverged at {field}");
    }
}

#[test]
fn destination_cuts_preserve_handoff_command_identity_and_accepted_queue_order() {
    let (seed, session) = handed_off_seed();
    let accepted = queue(&seed, &session);
    let script = json!({
        "version": 1, "repeat_last_step": true,
        "steps": [{"output": {"text": "verdict continuation complete"}, "finish": "stop"}]
    });
    let pilot = seed.fork(&script).with_session_lease_seconds(1);
    let trace = pilot.root.join("destination-commits.jsonl");
    let mut command = pilot.command();
    command
        .args(["agent", "--session", &session])
        .env(TRACE_ENV, &trace);
    let mut agent = AgentProcess::start_command(command, true);
    agent.wait_for_completed_turns(3);
    agent.send(&quit_frame());
    assert!(agent.finish().status.success());
    assert_eq!(dispatches(&pilot).len(), 3);
    assert_queue_order(&pilot, &session, &accepted);
    let rows: Vec<Value> = std::fs::read_to_string(trace)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .filter(|row: &Value| row["indexed"] == true)
        .collect();
    assert_ne!(rows, [] as [Value; 0]);
    eprintln!("destination verdict matrix: {} indexed cuts", rows.len());
    for target in rows {
        let cut = seed.fork(&script).with_session_lease_seconds(1);
        kill_at(&cut, &session, &target);
        let before = store(&cut)
            .load_intents(&session)
            .unwrap()
            .root_continuations;
        assert_queue_order(&cut, &session, &accepted);
        finish_reopen(&cut, &session);
        let first = store(&cut)
            .load_intents(&session)
            .unwrap()
            .root_continuations;
        assert_eq!(first.len(), 1, "one handoff decision: {target}");
        if !before.is_empty() {
            assert_eq!(first, before, "never mint a replacement command: {target}");
        }
        let sent = dispatches(&cut);
        assert!(sent.len() <= 3, "unexpected physical dispatch: {target}");
        finish_reopen(&cut, &session);
        assert_eq!(dispatches(&cut), sent, "second reopen dispatched: {target}");
        assert_eq!(
            store(&cut)
                .load_intents(&session)
                .unwrap()
                .root_continuations,
            first
        );
        assert_queue_order(&cut, &session, &accepted);
    }
}
