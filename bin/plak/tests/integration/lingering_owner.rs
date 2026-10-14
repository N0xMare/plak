//! The lingering owner (spec 05 feed and projector laws; spec 02 §2.6): a
//! host stopped past its runtime lease keeps running beside the owner that
//! took the session over. Nothing it does reaches the new owner's views or the
//! provider.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    missing_docs,
    reason = "private process-level test harness"
)]

use std::process::Command;
use std::time::{Duration, Instant};

use plak::home::paths::PlakPaths;
use plak::store::session_records::{SessionRecordStore, SqliteSessionStore};
use plakous_offline_crash_harness::evidence::{EvidenceKind, read_evidence};
use serde_json::{Value, json};

use super::offline_support::*;

const ANSWER: &str = "the owner's answer";

fn kill(pid: u32, signal: &str) {
    let status = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .status()
        .expect("kill spawned");
    assert!(status.success(), "kill -{signal} {pid} failed");
}

fn with_product_writer_excluded(paths: &PlakPaths, suspend: impl FnOnce()) {
    let mut database = rusqlite::Connection::open(paths.product_database()).unwrap();
    database.busy_timeout(WAIT).unwrap();
    // SIGSTOP while the host owns the WAL writer strands that lock past lease
    // expiry. Take the writer first so suspension tests lease fencing alone.
    let transaction = database
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    suspend();
    transaction.rollback().unwrap();
}

#[test]
fn owner_suspension_excludes_a_product_writer() {
    let fixture = Fixture::new(&json!({"version": 1}));
    let paths = PlakPaths::explicit(fixture.home());
    let _store = plak::store::ProductStore::open(&paths).unwrap();
    let mut competing = rusqlite::Connection::open(paths.product_database()).unwrap();
    competing.busy_timeout(Duration::ZERO).unwrap();
    with_product_writer_excluded(&paths, || {
        assert!(
            competing
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .is_err()
        );
    });
    assert!(
        competing
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .is_ok()
    );
}

fn dispatches(fixture: &Fixture) -> usize {
    read_evidence(fixture.root.join("evidence.jsonl"))
        .expect("physical dispatch evidence")
        .iter()
        .filter(|record| record.kind == EvidenceKind::ProviderDispatched)
        .count()
}

fn user_rows(views: &plakous_hosted::sessions::projector::SessionViews) -> Vec<String> {
    views
        .rows
        .values()
        .filter_map(|row| match row {
            plakous_hosted::sessions::TranscriptRow::User { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// Waits until the lingering host has answered its prompt frame: any record
/// after the frame that refuses, fails or ends its stream.
fn wait_for_the_refusal(agent: &AgentProcess, before: usize) -> Vec<Value> {
    let deadline = Instant::now() + WAIT;
    loop {
        let records: Vec<Value> = agent
            .snapshot()
            .iter()
            .skip(before)
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        if records.iter().any(|record| {
            matches!(
                record["kind"].as_str(),
                Some("error" | "bye" | "turn" | "notice")
            ) || record["phase"] == "idle"
        }) {
            return records;
        }
        assert!(
            Instant::now() < deadline,
            "the lingering host never answered its prompt: {records:#?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_lingering_owner_past_its_lease_changes_nothing_of_the_new_owner() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "repeat_last_step": true,
        "steps": [{"output": {"text": ANSWER}}]
    }));
    let mut stale = AgentProcess::start(&fixture, &[]);
    let hello = stale.wait_for("hello", |record| record["kind"] == "hello");
    let session = hello["session_id"].as_str().expect("session id").to_owned();
    stale.send(&prompt_frame("first prompt"));
    stale.wait_for_completed_turns(1);

    // A stopped process is a suspended owner: its lease expires while it
    // cannot renew, and the reopen below takes the session over.
    with_product_writer_excluded(&PlakPaths::explicit(fixture.home()), || {
        kill(stale.id(), "STOP");
        let pid = rustix::process::Pid::from_raw(stale.id().try_into().unwrap()).unwrap();
        let (stopped, status) =
            rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::UNTRACED)
                .unwrap()
                .unwrap();
        assert_eq!(stopped, pid);
        assert_eq!(
            status.stopping_signal(),
            Some(rustix::process::Signal::STOP.as_raw())
        );
    });
    let mut owner = reopen_until_the_lease_clears(&fixture, &session);
    owner.wait_for("the new owner's ready", |record| record["kind"] == "ready");
    let store = SqliteSessionStore::new(PlakPaths::explicit(fixture.home()));
    let taken = store
        .load_views(&session)
        .unwrap()
        .expect("the owner's views");
    let dispatched = dispatches(&fixture);
    assert_eq!(dispatched, 1, "one prompt, one physical dispatch");

    // The lingering owner resumes and acts: its heartbeat and projector
    // passes run, and it submits a prompt of its own.
    kill(stale.id(), "CONT");
    let before = stale.snapshot().len();
    stale.send(&prompt_frame("lingering prompt"));
    let answered = wait_for_the_refusal(&stale, before);
    assert!(
        !answered
            .iter()
            .any(|record| record["kind"] == "assistant" && record["text"] == ANSWER),
        "the lingering owner's prompt reached the model: {answered:#?}"
    );
    stale.send(&quit_frame());
    stale.close_stdin();
    let stale = stale.finish();
    assert_eq!(
        dispatches(&fixture),
        dispatched,
        "the lingering owner dispatched; its stream:\n{}\nstderr:\n{}",
        stale.lines.join("\n"),
        stale.stderr
    );
    let after = store
        .load_views(&session)
        .unwrap()
        .expect("the owner's views");
    assert_eq!(
        after.rows, taken.rows,
        "the lingering owner moved the views"
    );
    assert_eq!(after.state.counters, taken.state.counters);
    assert_eq!(after.cursor, taken.cursor);

    // The new owner still drives the session, and its next turn is the only
    // one it runs: the lingering prompt is neither executed nor resubmitted.
    owner.send(&prompt_frame("owner prompt"));
    owner.wait_for_completed_turns(1);
    owner.send(&quit_frame());
    let finished = owner.finish();
    assert!(finished.status.success(), "{}", finished.stderr);
    let mut reopened = reopen_until_the_lease_clears(&fixture, &session);
    reopened.wait_for("ready", |record| record["kind"] == "ready");
    reopened.send(&quit_frame());
    assert!(reopened.finish().status.success());
    assert_eq!(dispatches(&fixture), dispatched + 1);
    let last = store.load_views(&session).unwrap().expect("views");
    assert_eq!(user_rows(&last), ["first prompt", "owner prompt"]);
    assert_eq!(last.state.counters.root_turns, 2);
}
