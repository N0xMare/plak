//! Spec 02 section 2.6: the headless faces settle an interrupted carry and
//! follow a session a carry moved on to its successor, printing the note.

use super::*;
use plak::home::paths::PlakPaths;
use plak::store::session_records::{SessionRecordStore as _, SqliteSessionStore};

/// A source session the CLI created, and an unpublished carry of it written
/// ahead the way a crashed `/rebind` leaves it.
fn source_with_interrupted_carry(fixture: &Fixture) -> (String, String) {
    let first = run_cli(
        fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--print-session-id",
            "--",
            "before the carry",
        ],
    );
    assert!(first.status.success(), "{}", first.stderr);
    let source = first.session_id();
    let store = SqliteSessionStore::new(PlakPaths::explicit(fixture.home()));
    let record = store.load(&source).unwrap();
    let mut intent = plakous_hosted::sessions::SessionRecord::new(record.workspace_root.clone());
    intent.link_carried_forward(&source, &plak::home::state::state_key());
    intent.carry_intent = Some(plakous_hosted::sessions::CarryIntent {
        source: record.plakous_locator.expect("source locator"),
        continues_line: true,
        cause: "binding changed; carried into a new session".into(),
        orchestration: false,
    });
    store.save(&mut intent).unwrap();
    (source, intent.id)
}

/// `plak run --session <source>` finishes the interrupted carry, then runs
/// on the successor the source's line moved to, and says so.
#[test]
fn a_run_of_a_carried_session_lands_on_its_successor_with_the_note() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "steps": [{"output": {"text": "first answer"}}]
    }));
    let (source, successor) = source_with_interrupted_carry(&fixture);
    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"output": {"text": "after the carry"}}]
    }));
    let next = run_cli(
        &fixture,
        &[
            "run",
            "--session",
            &source,
            "--print-session-id",
            "--",
            "on the successor",
        ],
    );
    assert!(next.status.success(), "{}\n{}", next.stdout, next.stderr);
    assert_eq!(next.session_id(), successor, "{}", next.stderr);
    assert!(
        next.stderr.contains(&format!(
            "session {source} was carried forward as {successor}"
        )),
        "{}",
        next.stderr
    );
    assert!(next.stdout.contains("after the carry"), "{}", next.stdout);
    // The successor is bound and settled.
    let store = SqliteSessionStore::new(PlakPaths::explicit(fixture.home()));
    let record = store.load(&successor).unwrap();
    assert!(record.carry_intent.is_none());
    assert!(record.plakous_locator.is_some());
}

/// `plak agent --session <intent>` finishes the carry its record names and
/// attaches the successor, instead of creating an empty session.
#[test]
fn an_agent_on_an_interrupted_carry_finishes_it_and_attaches_the_successor() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "steps": [{"output": {"text": "first answer"}}]
    }));
    let (_source, successor) = source_with_interrupted_carry(&fixture);
    let mut agent = AgentProcess::start(&fixture, &["--session", &successor]);
    let hello = agent.wait_for("hello", |row| row["kind"] == "hello");
    assert_eq!(hello["session_id"], successor.as_str());
    agent.wait_for("ready", |row| row["kind"] == "ready");
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "{}", outcome.stderr);
    assert!(
        outcome.stderr.contains("finished the interrupted carry"),
        "{}",
        outcome.stderr
    );
    let store = SqliteSessionStore::new(PlakPaths::explicit(fixture.home()));
    let record = store.load(&successor).unwrap();
    assert!(record.carry_intent.is_none());
    assert!(record.plakous_locator.is_some());
}
