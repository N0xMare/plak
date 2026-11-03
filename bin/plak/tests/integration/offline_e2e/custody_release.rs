//! Spec 03 §14: a host that quits cleanly releases the custody of the agent
//! tree it held, on every face, so the next process takes the tree without a
//! takeover proof.

use super::*;
use plak::home::paths::PlakPaths;

const ANSWER: &str = "the tree root answered";

fn tree_answer_script() -> Value {
    json!({
        "version": 1,
        "tree": true,
        "durable_tree": true,
        "steps": [{"output": {"text": ANSWER}}]
    })
}

/// `(tree_id, released)` of every custody record in the fixture's tree store.
fn custody_records(fixture: &Fixture) -> Vec<(String, i64)> {
    let database = PlakPaths::explicit(fixture.home()).subagents_database();
    let connection =
        rusqlite::Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut statement = connection
        .prepare("SELECT tree_id, released FROM agent_tree_custody ORDER BY tree_id")
        .unwrap();
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn assert_released(fixture: &Fixture, session: Option<&str>) {
    let records = custody_records(fixture);
    assert!(!records.is_empty(), "the tree was never held");
    if let Some(session) = session {
        assert!(
            records.iter().any(|(tree_id, _)| tree_id == session),
            "no custody record for {session}: {records:?}"
        );
    }
    assert!(
        records.iter().all(|(_, released)| *released == 1),
        "a clean quit left custody held: {records:?}"
    );
}

#[test]
fn a_clean_run_releases_its_tree_custody() {
    let fixture = Fixture::new(&tree_answer_script());
    let run = run_cli(
        &fixture,
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
            "answer once",
        ],
    );
    assert!(run.status.success(), "{}", run.stderr);
    assert!(run.stdout.contains(ANSWER), "{}", run.stdout);
    assert_released(&fixture, Some(&run.session_id()));
}

#[test]
fn a_clean_agent_quit_releases_its_tree_custody() {
    let fixture = Fixture::new(&tree_answer_script());
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("answer once"));
    agent.wait_for("the answer", |record| record["kind"] == "assistant_done");
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "{}", outcome.stderr);
    assert_released(&fixture, Some(&outcome.session_id()));
}

#[test]
fn a_clean_tui_quit_releases_its_tree_custody() {
    let fixture = Fixture::new(&tree_answer_script());
    let mut terminal = TerminalChild::spawn(&fixture, 120, None);
    terminal.wait_screen("/10M");
    terminal.send(b"answer once\r");
    terminal.wait_screen(ANSWER);
    terminal.send(b"\x11");
    let (status, raw) = terminal.finish_with_raw();
    assert!(
        status.success(),
        "the TUI did not quit cleanly:\n{}",
        String::from_utf8_lossy(&raw)
    );
    assert_released(&fixture, None);
}
