//! A product child that itself has a descendant or a parked process.
//!
//! `tree_child_parked_job_loss`: the child is parked on one promoted local
//! process. A cut leaves that process unknown rather than started again.

use super::super::super::offline_support::{
    AgentOutcome, AgentProcess, Fixture, prompt_frame, quit_frame,
};
use super::super::{invariants, summary, trace};
use super::{FOLLOWUP, Lifecycle, Value};
use plakous_recursive_subagents::protocol::NodeStatus;
use plakous_recursive_subagents::tree::PersistedTree;
use serde_json::json;

const PARKED: &str = "crash-sweep-lineage-parked";
const CHILD_SCRIPT: &str = "child-script.json";
const GRAND_SCRIPT: &str = "grandchild-script.json";
const CHILD_EVIDENCE: &str = "child-evidence.jsonl";
const GRAND_EVIDENCE: &str = "grandchild-evidence.jsonl";
const CHILD_PARKED: &str = "lineage-parked";
const NEVER: &str = "lineage-never";
const MARKER: &str = "parked-job-starts";
const QUIET: std::time::Duration = std::time::Duration::from_secs(1);

const SCHEMA: &str = "{\"type\":\"object\",\"properties\":{\"summary\":{\"type\":\"string\"}},\"required\":[\"summary\"],\"additionalProperties\":false}";

fn root(fixture: &Fixture, steps: &Value) -> Value {
    json!({
        "version": 1,
        "tree": true,
        "durable_tree": true,
        "product_child_script": fixture.root.join(CHILD_SCRIPT),
        "repeat_last_step": true,
        "steps": steps
    })
}

fn spawn_step() -> Value {
    json!({"output": {"tool": {"name": "spawn_agent", "arguments": {
        "template": "review",
        "tier": "read_only",
        "description": "reviewer",
        "prompt": "review the diff",
        "output_schema_json": SCHEMA
    }}}, "finish": "tool_calls"})
}

pub(super) fn parked_script() -> Value {
    json!({"version": 1, "steps": [{"output": {"text": PARKED}}]})
}

pub(super) fn configure_parked(fixture: &Fixture) {
    let marker = fixture.root.join(MARKER);
    let quoted = format!("'{}'", marker.display().to_string().replace('\'', "'\\''"));
    // pi presents the process tool as `bash`. `timeout` 0 backgrounds it.
    // The canonical process_session_start arguments are not in that catalog,
    // so a call under that name is proven not dispatched.
    let command = format!("printf '%s\\n' started >> {quoted}; read line");
    let child = json!({
        "version": 1,
        "repeat_last_step": true,
        "steps": [
            {"output": {"tool": {"name": "bash", "arguments": {
                "command": command,
                "timeout": 0
            }}}, "finish": "tool_calls"},
            {"stream": [{"gate": {"file": fixture.root.join(NEVER)}}],
             "output": {"text": "child parked"}}
        ]
    });
    std::fs::write(
        fixture.root.join(CHILD_SCRIPT),
        serde_json::to_vec_pretty(&child).unwrap(),
    )
    .unwrap();
    let _ = std::fs::remove_file(fixture.root.join(GRAND_SCRIPT));
    fixture.write_script(&root(
        fixture,
        &json!([
            spawn_step(),
            {"stream": [{"gate": {"file": fixture.root.join(CHILD_PARKED)}}],
             "output": {"text": PARKED}}
        ]),
    ));
}

fn evidence_dispatches(fixture: &Fixture, name: &str) -> usize {
    std::fs::read_to_string(fixture.root.join(name))
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("provider_dispatched"))
        .count()
}

fn marker_lines(fixture: &Fixture) -> usize {
    std::fs::read_to_string(fixture.root.join(MARKER))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.is_empty())
        .count()
}

fn session(fixture: &Fixture) -> String {
    use plak::store::session_records::SessionRecordStore as _;
    plak::store::session_records::SqliteSessionStore::new(summary::paths(fixture))
        .list_for_workspace(&fixture.workspace())
        .unwrap()
        .pop()
        .unwrap()
        .id
}

fn tree(fixture: &Fixture, session: &str) -> Option<PersistedTree> {
    match plakous_recursive_subagents_store::SqliteTreeStore::inspect(
        &summary::paths(fixture).subagents_database(),
        session,
    ) {
        Ok(plakous_recursive_subagents_store::TreeInspection::Tree(tree)) => Some(*tree),
        _ => None,
    }
}

fn node_status(fixture: &Fixture, agent: &str) -> Option<NodeStatus> {
    tree(fixture, &session(fixture))
        .and_then(|tree| tree.rows().node(agent).ok().map(|node| node.status))
}

/// Prompts, lets the root answer once the descendant or the parked process
/// is durable and the stores have gone quiet, then quits.
pub(super) fn lifecycle(
    fixture: &Fixture,
    records: &[Value],
    state: &mut Lifecycle,
    parked_job: bool,
) -> Option<Value> {
    if !state.sent && records.iter().any(|row| row["kind"] == "ready") {
        state.sent = true;
        return Some(prompt_frame("crash-sweep-original-prompt"));
    }
    let ready = if parked_job {
        marker_lines(fixture) >= 1
    } else {
        evidence_dispatches(fixture, GRAND_EVIDENCE) >= 1
    };
    let parked = fixture.root.join(CHILD_PARKED);
    if state.sent && !parked.exists() && ready {
        let len =
            std::fs::read_to_string(fixture.root.join("commit-trace.jsonl")).map_or(0, |trace| {
                trace
                    .lines()
                    .filter(|line| line.contains("\"indexed\":true"))
                    .count()
            });
        match state.trace_quiet {
            Some((seen, since)) if seen == len => {
                if since.elapsed() >= QUIET {
                    std::fs::write(&parked, b"").unwrap();
                }
            }
            _ => state.trace_quiet = Some((len, std::time::Instant::now())),
        }
    }
    let answered = state.sent
        && parked.exists()
        && super::face_idle_after(records, |row| {
            row["kind"] == "assistant"
                && row["text"]
                    .as_str()
                    .is_some_and(|text| text.contains(PARKED))
        });
    if !answered || state.quit {
        return None;
    }
    state.quit = true;
    Some(quit_frame())
}

pub(super) fn parked_lifecycle(
    fixture: &Fixture,
    records: &[Value],
    state: &mut Lifecycle,
) -> Option<Value> {
    lifecycle(fixture, records, state, true)
}

pub(super) fn assert_parked(fixture: &Fixture, outcome: &AgentOutcome, _: &summary::Summary) {
    assert!(
        outcome
            .assistant_text()
            .iter()
            .any(|text| text.contains(PARKED))
    );
    assert_eq!(marker_lines(fixture), 1, "the parked job started once");
    assert!(node_status(fixture, "c1").is_some());
}

fn follow_up(fixture: &Fixture) {
    let _ = std::fs::remove_file(fixture.root.join(CHILD_PARKED));
    let waiting = json!({
        "version": 1,
        "repeat_last_step": true,
        "steps": [{"stream": [{"gate": {"file": fixture.root.join(NEVER)}}],
                   "output": {"text": "held"}}]
    });
    std::fs::write(
        fixture.root.join(CHILD_SCRIPT),
        serde_json::to_vec_pretty(&waiting).unwrap(),
    )
    .unwrap();
    std::fs::write(
        fixture.root.join(GRAND_SCRIPT),
        serde_json::to_vec_pretty(&waiting).unwrap(),
    )
    .unwrap();
    fixture.write_script(&root(
        fixture,
        &json!([{"output": {"text": FOLLOWUP}, "finish": "stop"}]),
    ));
}

/// One cut's relaunches. The descendant and the parked process stay as the
/// cut left them: one start, no second, and a second relaunch adds neither.
pub(in super::super) fn after_kill(
    fixture: &Fixture,
    _session: &str,
    commits: &[trace::Commit],
    parked_job: bool,
    relaunch: impl Fn(&str) -> AgentProcess,
) -> Value {
    let grand_before = evidence_dispatches(fixture, GRAND_EVIDENCE);
    let marker_before = marker_lines(fixture);
    let child_before = evidence_dispatches(fixture, CHILD_EVIDENCE);
    let kept = node_status(fixture, if parked_job { "c1" } else { "c2" });
    if !parked_job {
        assert_ne!(
            kept,
            Some(NodeStatus::RecoveryRequired),
            "the cut left the grandchild recovery_required"
        );
    }
    follow_up(fixture);
    let root_before = invariants::dispatches(fixture).len();
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
    if parked_job {
        assert_eq!(
            marker_lines(fixture),
            marker_before,
            "a parked job started again"
        );
    } else {
        assert_eq!(
            evidence_dispatches(fixture, GRAND_EVIDENCE),
            grand_before,
            "the grandchild dispatched again"
        );
        // Loss marking may move a kept node from running to interrupted.
        // The cut dropped it only when the node itself is gone.
        assert!(
            node_status(fixture, "c2").is_some() || grand_before == 0,
            "the cut dropped the grandchild"
        );
    }
    assert!(
        evidence_dispatches(fixture, CHILD_EVIDENCE) <= child_before + usize::from(requeued),
        "the child dispatched again"
    );
    let root_after = invariants::dispatches(fixture).len();
    assert_eq!(root_after, root_before + 1 + usize::from(requeued));
    invariants::no_duplicate_dispatch(fixture);
    let mut second = relaunch("relaunch-2");
    second.send(&quit_frame());
    assert!(second.finish().status.success());
    if parked_job {
        assert_eq!(marker_lines(fixture), marker_before);
    } else {
        assert_eq!(evidence_dispatches(fixture, GRAND_EVIDENCE), grand_before);
    }
    assert_eq!(invariants::dispatches(fixture).len(), root_after);
    let running = if parked_job {
        marker_before >= 1
    } else {
        grand_before >= 1
    };
    json!({
        "outcome": if running {
            "HELD"
        } else if requeued {
            "REQUEUED"
        } else {
            "NOT_HANDED_OFF"
        },
        "invariants": true,
        "dispatches": root_after,
        "global_order": commits,
        "global_ordinal": commits.last().unwrap().ordinal,
    })
}
