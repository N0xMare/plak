//! Spec 03 section 4.4: wait settlement is a projection of the caller's feed,
//! live. A root wait whose call the root's history has committed retires its
//! durable receipt while the root's turn is still running, not only at that
//! turn's terminal.

use super::*;
use plak::home::paths::PlakPaths;

const STILL_WORKING: &str = "the root keeps working after its wait";

fn retained_wait_receipts(database: &Path) -> i64 {
    let connection =
        rusqlite::Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    connection
        .query_row(
            "SELECT count(*) FROM agent_operations WHERE tool_name='wait_agent'",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

fn spawn_step(description: &str) -> Value {
    json!({"output": {"tool": {"name": "spawn_agent", "arguments": {
        "template": "implement",
        "tier": "mutate",
        "workspace": "fork",
        "description": description,
        "prompt": "make the offline scripted change",
        "output_schema_json": "{\"type\":\"object\",\"properties\":{\"summary\":{\"type\":\"string\"}},\"required\":[\"summary\"],\"additionalProperties\":false}"
    }}}, "finish": "tool_calls"})
}

/// The round with the waits holds the receipt durably for a few seconds
/// before the next round can start: after the first wait returns the
/// settled `c1`, a second, serial wait on the held `c2` times out. The next
/// round's pass must then retire that receipt while the turn still runs.
#[test]
fn a_running_root_turn_retires_its_committed_wait_receipts() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "tree": true,
        "durable_tree": true,
        "child_hold_ms": 15_000,
        "child_hold_after": 1,
        "steps": [
            spawn_step("settled scripted child"),
            spawn_step("held scripted child"),
            {"output": {"tools": [
                {"name": "wait_agent", "arguments": {"agent_ids": ["c1"]}},
                {"name": "wait_agent", "arguments": {"agent_ids": ["c2"], "timeout_ms": 3_000}}
            ]}, "finish": "tool_calls"},
            {"stream": [{"text": STILL_WORKING}, {"stall": 60_000}]}
        ]
    }));
    let database = PlakPaths::explicit(fixture.home()).subagents_database();
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("delegate, wait, then keep working"));
    // Witness: the settled wait's receipt is durable while the second wait
    // holds the round open.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut witnessed = false;
    while !witnessed && Instant::now() < deadline {
        witnessed = database.exists() && retained_wait_receipts(&database) == 1;
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(witnessed, "the settled wait never held a durable receipt");
    // The stalled round starts only after both waits' results committed to
    // the root's history, and it holds the root's turn open.
    agent.wait_for("the round after the waits", |record| {
        record["kind"] == "assistant"
            && record["text"]
                .as_str()
                .is_some_and(|text| text.contains(STILL_WORKING))
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut retained = retained_wait_receipts(&database);
    while retained != 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        retained = retained_wait_receipts(&database);
    }
    let running = !agent
        .snapshot()
        .iter()
        .any(|line| line.contains("\"assistant_done\""));
    assert!(
        running,
        "the root turn ended before the receipt was checked"
    );
    assert_eq!(
        retained, 0,
        "a committed root wait kept its receipt while the turn ran"
    );
    agent.send(&quit_frame());
    let _ = agent.finish();
}
