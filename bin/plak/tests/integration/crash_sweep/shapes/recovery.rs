//! Root fences, a transient continuation, and custody of a dead holder.
//!
//! `node_fenced_turn` fences the root with an in-band outcome nobody can
//! observe. The live host recovers that turn in place; quit waits until the
//! `recovered` record, because a quit already deferred at idle skips it.
//!
//! `node_fenced_turn_child` is the same fault on a product child. Eager
//! in-place recovery is root-only, so the child may stay `recovery_required`.
//! It is not dropped, and it is not dispatched again.
//!
//! `root_transient_resume_cut` quits while the automatic-continuation notice
//! is up and the provider has dispatched once. A cut continues that decision
//! at most once. A second relaunch continues nothing more.
//!
//! `custody_takeover` is an ordinary durable-tree answer. A cut that leaves
//! the holder dead is taken by the relaunch: exit 0 and, when the row was
//! unreleased, a higher fence. Exit 7 is a finding, not a recovered class.

use super::super::super::offline_support::{
    AgentOutcome, AgentProcess, Fixture, prompt_frame, quit_frame,
};
use super::super::{invariants, summary, trace};
use super::{ANSWER, Lifecycle, Value};
use serde_json::json;

pub(super) fn fenced_script() -> Value {
    json!({
        "version": 1,
        "steps": [
            {"stream": [{"text": "fence-prefix"}],
             "fault": {"at": "in_band", "kind": "outcome_unknown", "after_events": 2}},
            {"output": {"text": ANSWER}}
        ]
    })
}

pub(super) fn configure_fenced(_: &Fixture) {}

fn script_still_fences(fixture: &Fixture) -> bool {
    std::fs::read_to_string(fixture.script())
        .unwrap_or_default()
        .contains("outcome_unknown")
}

pub(super) fn fenced_lifecycle(
    fixture: &Fixture,
    records: &[Value],
    state: &mut Lifecycle,
) -> Option<Value> {
    if !state.sent && records.iter().any(|row| row["kind"] == "ready") {
        state.sent = true;
        return Some(prompt_frame("crash-sweep-original-prompt"));
    }
    if state.sent && !state.quit && records.iter().any(|row| row["kind"] == "recovered") {
        state.quit = true;
        return Some(quit_frame());
    }
    // A requeued original is rewritten to a plain answer, which never emits
    // `recovered`. The live script still carries the fault, so this idle
    // quit cannot skip the in-place reopen.
    if state.sent
        && !state.quit
        && !script_still_fences(fixture)
        && super::face_idle_after(records, |row| row["kind"] == "assistant_done")
    {
        state.quit = true;
        return Some(quit_frame());
    }
    None
}

pub(super) fn assert_fenced(_: &Fixture, outcome: &AgentOutcome, state: &summary::Summary) {
    let recovered = outcome
        .records()
        .into_iter()
        .find(|row| row["kind"] == "recovered")
        .expect("the live host recovered the fenced root");
    assert_eq!(recovered["claim"], "model_request");
    assert!(
        outcome
            .records()
            .iter()
            .any(|row| row["kind"] == "turn_lifecycle" && row["phase"] == "recovery_required")
    );
    assert!(!state.fenced_turns.is_empty(), "the fence is durable");
    let _ = outcome;
}

const CHILD_SCRIPT: &str = "child-script.json";

pub(super) fn child_fenced_script() -> Value {
    json!({"version": 1, "steps": [{"output": {"text": "child fence"}}]})
}

const CHILD_FAULT_GO: &str = "child-fault-go";

pub(super) fn configure_child_fenced(fixture: &Fixture) {
    // The step gate holds the fault until the spawn's commits are quiet.
    // An in-band fault ignores stream gates, so the hold has to be the
    // step's own dispatch gate.
    let mut child = fenced_script();
    child["steps"][0]["gate"] = json!({ "file": fixture.root.join(CHILD_FAULT_GO) });
    std::fs::write(
        fixture.root.join(CHILD_SCRIPT),
        serde_json::to_vec_pretty(&child).unwrap(),
    )
    .unwrap();
    fixture.write_script(&json!({
        "version": 1,
        "tree": true,
        "durable_tree": true,
        "product_child_script": fixture.root.join(CHILD_SCRIPT),
        "repeat_last_step": true,
        "steps": [
            {"output": {"tool": {"name": "spawn_agent", "arguments": {
                "template": "review",
                "tier": "read_only",
                "description": "reviewer",
                "prompt": "review the diff",
                "output_schema_json": "{\"type\":\"object\",\"properties\":{\"summary\":{\"type\":\"string\"}},\"required\":[\"summary\"],\"additionalProperties\":false}"
            }}}, "finish": "tool_calls"},
            {"stream": [{"gate": {"file": fixture.root.join("child-fenced")}}],
             "output": {"text": "child fence held"}}
        ]
    }));
}

fn child_dispatches(fixture: &Fixture) -> usize {
    std::fs::read_to_string(fixture.root.join("child-evidence.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("provider_dispatched"))
        .count()
}

fn indexed_commits(fixture: &Fixture) -> usize {
    std::fs::read_to_string(fixture.root.join("commit-trace.jsonl")).map_or(0, |trace| {
        trace
            .lines()
            .filter(|line| line.contains("\"indexed\":true"))
            .count()
    })
}

fn trace_is_quiet(fixture: &Fixture, state: &mut Lifecycle, quiet: std::time::Duration) -> bool {
    let len = indexed_commits(fixture);
    match state.trace_quiet {
        Some((seen, since)) if seen == len => since.elapsed() >= quiet,
        _ => {
            state.trace_quiet = Some((len, std::time::Instant::now()));
            false
        }
    }
}

pub(super) fn child_fenced_lifecycle(
    fixture: &Fixture,
    records: &[Value],
    state: &mut Lifecycle,
) -> Option<Value> {
    if !state.sent && records.iter().any(|row| row["kind"] == "ready") {
        state.sent = true;
        return Some(prompt_frame("crash-sweep-original-prompt"));
    }
    let fault_go = fixture.root.join(CHILD_FAULT_GO);
    let gate = fixture.root.join("child-fenced");
    // Dispatch is recorded before the step gate. Waiting out the spawn's
    // commits before releasing the fault keeps that fault off the root's
    // tool-result transactions; the two pilots otherwise swap a checkpoint
    // and a session release.
    if state.stage == 0
        && child_dispatches(fixture) >= 1
        && trace_is_quiet(fixture, state, std::time::Duration::from_secs(1))
    {
        std::fs::write(&fault_go, b"").unwrap();
        state.stage = 1;
        state.trace_quiet = None;
    }
    if state.stage == 1 && trace_is_quiet(fixture, state, std::time::Duration::from_secs(1)) {
        std::fs::write(&gate, b"").unwrap();
        state.stage = 2;
        state.trace_quiet = None;
    }
    let answered = state.stage == 2
        && gate.exists()
        && super::face_idle_after(records, |row| {
            row["kind"] == "assistant"
                && row["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("child fence held"))
        });
    if answered {
        state.stage = 3;
        state.trace_quiet = None;
    }
    if state.stage == 3
        && !state.quit
        && trace_is_quiet(fixture, state, std::time::Duration::from_secs(1))
    {
        state.quit = true;
        return Some(quit_frame());
    }
    None
}

pub(super) fn assert_child_fenced(fixture: &Fixture, outcome: &AgentOutcome, _: &summary::Summary) {
    assert!(
        outcome
            .assistant_text()
            .iter()
            .any(|text| text.contains("child fence held"))
    );
    assert_eq!(child_dispatches(fixture), 1);
}

pub(in super::super) fn child_fenced_after_kill(
    fixture: &Fixture,
    _session: &str,
    commits: &[trace::Commit],
    relaunch: impl Fn(&str) -> AgentProcess,
) -> Value {
    let before = child_dispatches(fixture);
    fixture.write_script(&json!({
        "version": 1,
        "tree": true,
        "durable_tree": true,
        "product_child_script": fixture.root.join(CHILD_SCRIPT),
        "repeat_last_step": true,
        "steps": [{"output": {"text": super::FOLLOWUP}, "finish": "stop"}]
    }));
    std::fs::write(
        fixture.root.join(CHILD_SCRIPT),
        serde_json::to_vec_pretty(&json!({
            "version": 1,
            "repeat_last_step": true,
            "steps": [{"stream": [{"gate": {"file": fixture.root.join("child-fenced-held")}}],
                       "output": {"text": "held"}}]
        }))
        .unwrap(),
    )
    .unwrap();
    let mut reopened = relaunch("relaunch-1");
    let restored = reopened.wait_for("the restored queue", |row| row["kind"] == "queue");
    let requeued = restored["items"]
        .as_array()
        .is_some_and(|items| !items.is_empty());
    if requeued {
        reopened.send(&json!({"v": 1, "kind": "resume_queue"}));
        reopened.wait_for_completed_turns(1);
    }
    reopened.send(&prompt_frame("crash-sweep-followup-prompt"));
    reopened.wait_for_completed_turns(1 + usize::from(requeued));
    std::thread::sleep(std::time::Duration::from_millis(500));
    reopened.send(&quit_frame());
    let first = reopened.finish();
    assert!(first.status.success(), "follow-up failed: {}", first.stderr);
    assert_eq!(
        child_dispatches(fixture),
        before,
        "the fenced child dispatched again"
    );
    let mut second = relaunch("relaunch-2");
    second.send(&quit_frame());
    assert!(second.finish().status.success());
    assert_eq!(child_dispatches(fixture), before);
    json!({
        "outcome": if before >= 1 { "HELD" } else { "NOT_HANDED_OFF" },
        "invariants": true,
        "global_order": commits,
        "global_ordinal": commits.last().unwrap().ordinal,
    })
}

pub(super) fn transient_script() -> Value {
    json!({
        "version": 1,
        "steps": [
            {"stream": [{"text": "partial"}], "fault": {
                "at": "in_band",
                "kind": "transport",
                "phase": "Stream",
                "retryability": {"Eligible": {"retry_after": 1000}},
                "after_events": 1
            }},
            {"output": {"text": ANSWER}}
        ]
    })
}

pub(super) fn configure_transient(_: &Fixture) {}

fn notice_text(row: &Value) -> Option<&str> {
    row["text"]
        .as_str()
        .or_else(|| row["detail"].as_str())
        .or_else(|| row["message"].as_str())
}

fn continuation_notice(records: &[Value]) -> bool {
    records.iter().any(|row| {
        notice_text(row).is_some_and(|text| text.contains("will continue automatically"))
    })
}

pub(super) fn transient_lifecycle(
    fixture: &Fixture,
    records: &[Value],
    state: &mut Lifecycle,
) -> Option<Value> {
    if !state.sent && records.iter().any(|row| row["kind"] == "ready") {
        state.sent = true;
        state.baseline_dispatches = invariants::dispatches(fixture).len();
        return Some(prompt_frame("crash-sweep-original-prompt"));
    }
    // Quit on the same poll as the notice races the failure's wake-suppression
    // intent, and the two pilots then disagree. The continuation itself is
    // scheduled a second out, so a short quiet wait lands the intent without
    // letting that retry dispatch.
    if state.sent
        && !state.quit
        && continuation_notice(records)
        && invariants::dispatches(fixture).len() == state.baseline_dispatches + 1
        && trace_is_quiet(fixture, state, std::time::Duration::from_millis(250))
    {
        state.quit = true;
        return Some(quit_frame());
    }
    None
}

pub(super) fn assert_transient(fixture: &Fixture, outcome: &AgentOutcome, _: &summary::Summary) {
    assert!(
        outcome
            .records()
            .iter()
            .any(|row| notice_text(row)
                .is_some_and(|text| text.contains("will continue automatically"))),
        "the continuation notice was not up"
    );
    assert_eq!(
        invariants::dispatches(fixture).len(),
        1,
        "the continuation must not have dispatched before the cut"
    );
}

fn continuation_rows(fixture: &Fixture) -> usize {
    summary::read(fixture)
        .submissions
        .iter()
        .filter(|row| {
            row["command"]
                .as_str()
                .is_some_and(|command| command.starts_with("plak:resume-transient:root:"))
        })
        .count()
}

pub(in super::super) fn transient_after_kill(
    fixture: &Fixture,
    _session: &str,
    commits: &[trace::Commit],
    relaunch: impl Fn(&str) -> AgentProcess,
) -> Value {
    let before = invariants::dispatches(fixture).len();
    let mut followup = json!({
        "version": 1,
        "repeat_last_step": true,
        "steps": [{"output": {"text": super::FOLLOWUP}, "finish": "stop"}]
    });
    // Keep the fault off the follow-up. A repeated fault would plan a second continuation.
    fixture.write_script(&followup);
    let mut reopened = relaunch("relaunch-1");
    let restored = reopened.wait_for("the restored queue", |row| row["kind"] == "queue");
    let requeued = restored["items"]
        .as_array()
        .is_some_and(|items| !items.is_empty());
    if requeued {
        reopened.send(&json!({"v": 1, "kind": "resume_queue"}));
        reopened.wait_for_completed_turns(1);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < deadline
        && continuation_rows(fixture) == 0
        && invariants::dispatches(fixture).len() == before
    {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    reopened.send(&prompt_frame("crash-sweep-followup-prompt"));
    reopened.wait_for_completed_turns(1);
    reopened.send(&quit_frame());
    let first = reopened.finish();
    assert!(first.status.success(), "follow-up failed: {}", first.stderr);
    let continued = continuation_rows(fixture);
    assert!(
        continued <= 1,
        "more than one transient continuation: {continued}"
    );
    let after = invariants::dispatches(fixture).len();
    assert!(
        after <= before + 1 + usize::from(continued > 0),
        "dispatches {before} -> {after} with {continued} continuations"
    );
    invariants::no_duplicate_dispatch(fixture);
    followup["steps"] = json!([{"output": {"text": super::FOLLOWUP}, "finish": "stop"}]);
    fixture.write_script(&followup);
    let mut second = relaunch("relaunch-2");
    second.send(&quit_frame());
    assert!(second.finish().status.success());
    assert_eq!(continuation_rows(fixture), continued);
    assert_eq!(invariants::dispatches(fixture).len(), after);
    json!({
        "outcome": if continued == 1 { "CONTINUED" } else { "NOT_HANDED_OFF" },
        "invariants": true,
        "dispatches": after,
        "global_order": commits,
        "global_ordinal": commits.last().unwrap().ordinal,
    })
}

pub(super) fn custody_script() -> Value {
    json!({
        "version": 1,
        "tree": true,
        "durable_tree": true,
        "steps": [{"output": {"text": ANSWER}}]
    })
}

pub(super) fn configure_custody(_: &Fixture) {}

pub(super) fn assert_custody(fixture: &Fixture, outcome: &AgentOutcome, _: &summary::Summary) {
    assert!(
        outcome
            .assistant_text()
            .iter()
            .any(|text| text.contains(ANSWER))
    );
    let holder = holder(fixture);
    assert_eq!(holder.2, 1, "a clean quit left custody held: {holder:?}");
}

/// `(tree_id, fence, released)` of the fixture's one custody row.
pub(in super::super) fn holder(fixture: &Fixture) -> (String, i64, i64) {
    let database = summary::paths(fixture).subagents_database();
    let connection =
        rusqlite::Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut rows = connection
        .prepare("SELECT tree_id, fence, released FROM agent_tree_custody ORDER BY tree_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 1, "expected one custody row: {rows:?}");
    rows.pop().unwrap()
}

pub(in super::super) fn assert_taken(fixture: &Fixture, before: &(String, i64, i64)) {
    let after = holder(fixture);
    assert_eq!(after.0, before.0);
    if before.2 == 0 {
        assert!(
            after.1 > before.1,
            "a dead holder was not taken: fence {} -> {}",
            before.1,
            after.1
        );
    }
}
