//! A root whose child is a real product child session (spec 03 §14.1),
//! spawned, released and reopened through the product's own child route.
//!
//! `handoff_resume_child`: the child's turn is running on an idle root when
//! the host quits as a planned handoff. A cut after the child's handoff row
//! is durable relaunches into one continuation of the child, by itself;
//! any earlier cut leaves an owner-lost child held for the operator.
//!
//! `owner_lost_child_continue`: the same tree under `resume_after_loss =
//! "continue"`. The child's gate opens once the root answered, and the
//! face quits plainly once the child's attempt ended, so no live turn
//! races the release. A cut that loses the child's running request
//! continues it at most once: one the custody take's recovery rejoins is
//! not continued.
//!
//! Either way every continuation is one accepted continuation command and
//! one request, and a second relaunch continues nothing more.

use super::super::super::offline_support::{
    AgentOutcome, AgentProcess, Fixture, prompt_frame, quit_frame,
};
use super::super::{invariants, summary, trace};
use super::{FOLLOWUP, Lifecycle, Value};
use plakous_host_protocol::views::InterruptCauseView;
use plakous_recursive_subagents::protocol::NodeStatus;
use plakous_recursive_subagents::tree::{ContinuationCause, PersistedTree};
use serde_json::json;

/// The root's answer once its child's request is in flight.
const PARKED: &str = "crash-sweep-child-is-running";

/// The child's streamed prefix before it waits.
const CHILD_PREFIX: &str = "crash-sweep-child-working";

/// The file the root's second request waits on: written once the child's
/// request is in flight, so the root's turn ends with the child running.
const CHILD_PARKED: &str = "child-parked";

const CHILD_SCRIPT: &str = "child-script.json";

/// How long every store must stay still before the root may answer.
const QUIET: std::time::Duration = std::time::Duration::from_secs(1);

const CHILD_EVIDENCE: &str = "child-evidence.jsonl";

/// A gate no test ever opens: a child request waits on it until its host
/// releases or loses it.
const NEVER: &str = "child-never-released";

/// The owner-lost variant's child gate, opened once the root has answered,
/// so its child finishes before the quit and no live turn races the
/// release.
const CHILD_RELEASE: &str = "child-release";

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

fn waiting_child(fixture: &Fixture, gate: &str) -> Value {
    json!({
        "version": 1,
        "repeat_last_step": true,
        "steps": [{"stream": [{"text": CHILD_PREFIX}, {"gate": {"file": fixture.root.join(gate)}}],
                   "output": {"text": "child done"}}]
    })
}

/// The scripts are rewritten with this fixture's own paths by `configure`.
pub(super) fn script() -> Value {
    json!({"version": 1, "steps": [{"output": {"text": PARKED}}]})
}

pub(super) fn configure_handoff(fixture: &Fixture) {
    write_scripts(fixture, NEVER);
}

pub(super) fn configure_owner_lost(fixture: &Fixture) {
    write_scripts(fixture, CHILD_RELEASE);
    let path = fixture.home().join("config.toml");
    let config = std::fs::read_to_string(&path).unwrap();
    if !config.contains("resume_after_loss") {
        std::fs::write(
            &path,
            format!("{config}\n[host]\nresume_after_loss = \"continue\"\n"),
        )
        .unwrap();
    }
}

fn write_child(fixture: &Fixture, gate: &str) {
    std::fs::write(
        fixture.root.join(CHILD_SCRIPT),
        serde_json::to_vec_pretty(&waiting_child(fixture, gate)).unwrap(),
    )
    .unwrap();
}

fn write_scripts(fixture: &Fixture, gate: &str) {
    write_child(fixture, gate);
    fixture.write_script(&root(
        fixture,
        &json!([
            {"output": {"tool": {"name": "spawn_agent", "arguments": {
                "template": "review",
                "tier": "read_only",
                "description": "reviewer",
                "prompt": "review the diff",
                "output_schema_json": "{\"type\":\"object\",\"properties\":{\"summary\":{\"type\":\"string\"}},\"required\":[\"summary\"],\"additionalProperties\":false}"
            }}}, "finish": "tool_calls"},
            {"stream": [{"gate": {"file": fixture.root.join(CHILD_PARKED)}}],
             "output": {"text": PARKED}}
        ]),
    ));
}

pub(in super::super) fn child_dispatches(fixture: &Fixture) -> usize {
    std::fs::read_to_string(fixture.root.join(CHILD_EVIDENCE))
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("provider_dispatched"))
        .count()
}

/// Prompts; once the child's request is in flight lets the root answer;
/// quits once the root is idle with its child still running.
pub(super) fn handoff_lifecycle(
    fixture: &Fixture,
    records: &[Value],
    state: &mut Lifecycle,
) -> Option<Value> {
    lifecycle(fixture, records, state, false)
}

/// As the handoff lifecycle, but once the root answered it opens the
/// child's gate and quits only after the child's attempt ended.
pub(super) fn owner_lost_lifecycle(
    fixture: &Fixture,
    records: &[Value],
    state: &mut Lifecycle,
) -> Option<Value> {
    lifecycle(fixture, records, state, true)
}

fn lifecycle(
    fixture: &Fixture,
    records: &[Value],
    state: &mut Lifecycle,
    finish_child: bool,
) -> Option<Value> {
    if !state.sent && records.iter().any(|row| row["kind"] == "ready") {
        state.sent = true;
        return Some(prompt_frame("crash-sweep-original-prompt"));
    }
    // The root answers only once the child's request is in flight and every
    // store has gone quiet behind it: the child's runtime and the tree then
    // stand still until the quit, in every run.
    let parked = fixture.root.join(CHILD_PARKED);
    if state.sent && !parked.exists() && child_dispatches(fixture) > 0 {
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
    // The tree has folded the child's acceptance before the quit, so the
    // releasing host's projection finds the same feed in every run.
    let answered = state.sent
        && child_accepted(fixture)
        && super::face_idle_after(records, |row| {
            row["kind"] == "assistant"
                && row["text"]
                    .as_str()
                    .is_some_and(|text| text.contains(PARKED))
        });
    if !answered || state.quit {
        return None;
    }
    if finish_child {
        let release = fixture.root.join(CHILD_RELEASE);
        if !release.exists() {
            std::fs::write(&release, b"").unwrap();
        }
        // The root's host has also taken the child's result: what the
        // product store keeps of it does not race the quit.
        if !child_ended(fixture) || !result_noticed(fixture) {
            return None;
        }
    }
    state.quit = true;
    Some(quit_frame())
}

fn result_noticed(fixture: &Fixture) -> bool {
    rusqlite::Connection::open(summary::paths(fixture).product_database())
        .and_then(|store| {
            store.query_row(
                "SELECT count(*) FROM operator_notices WHERE notice_id LIKE '%:result:%'",
                [],
                |row| row.get::<_, i64>(0),
            )
        })
        .is_ok_and(|count| count > 0)
}

fn child_ended(fixture: &Fixture) -> bool {
    tree(fixture, &session(fixture)).is_some_and(|tree| {
        tree.rows().attempt("c1").is_some_and(|attempt| {
            attempt.status != plakous_recursive_subagents::protocol::AttemptStatus::Running
        })
    })
}

fn child_accepted(fixture: &Fixture) -> bool {
    tree(fixture, &session(fixture)).is_some_and(|tree| {
        tree.rows().commands().values().any(|command| {
            command.recipient == "c1"
                && command.fact == Some(plakous_recursive_subagents::tree::CommandFact::Accepted)
        })
    })
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

/// What the rows say about the child before any relaunch reads them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Before {
    /// No child row, or a child whose request never ran.
    NotRunning,
    /// A running child request the cut lost with its host.
    Running,
    /// A durable handoff of the child's running request.
    HandedOff,
    /// An interrupted child with no continuation.
    Interrupted,
}

fn before(fixture: &Fixture, session: &str) -> Before {
    let Some(tree) = tree(fixture, session) else {
        return Before::NotRunning;
    };
    let rows = tree.rows();
    let Ok(node) = rows.node("c1") else {
        return Before::NotRunning;
    };
    if let Some(continuation) = rows.loss_continuation("c1") {
        return match continuation.cause {
            ContinuationCause::Handoff => Before::HandedOff,
            ContinuationCause::OwnerLost | ContinuationCause::Transient => Before::Running,
        };
    }
    let turn_bound = rows
        .attempt("c1")
        .is_some_and(|attempt| attempt.turn_id.is_some() && attempt.output_json.is_none());
    match node.status {
        NodeStatus::Running if turn_bound => Before::Running,
        NodeStatus::Interrupted => Before::Interrupted,
        _ => Before::NotRunning,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in super::super) enum Variant {
    Handoff,
    OwnerLost,
}

/// The live run: the root answered with its child running, and the child
/// dispatched one request.
pub(super) fn assert_handoff_live(fixture: &Fixture, outcome: &AgentOutcome, _: &summary::Summary) {
    assert_live(fixture, outcome);
    let session = session(fixture);
    let tree = tree(fixture, &session).expect("the pilot persists its tree");
    assert_eq!(
        tree.rows()
            .loss_continuation("c1")
            .map(|continuation| continuation.cause),
        Some(ContinuationCause::Handoff),
        "the child's handoff is recorded before its release"
    );
}

pub(super) fn assert_owner_lost_live(
    fixture: &Fixture,
    outcome: &AgentOutcome,
    _: &summary::Summary,
) {
    assert_live(fixture, outcome);
    let session = session(fixture);
    let tree = tree(fixture, &session).expect("the pilot persists its tree");
    assert_ne!(
        tree.rows().attempt("c1").map(|attempt| attempt.status),
        Some(plakous_recursive_subagents::protocol::AttemptStatus::Running),
        "the child finished before the quit"
    );
    let view = plakous_recursive_subagents::tree::view::project_tree_view(tree.rows());
    let child = view
        .nodes
        .iter()
        .find(|node| node.agent_id == "c1")
        .expect("the pilot's child");
    assert_ne!(
        child.interrupt_cause,
        Some(InterruptCauseView::Handoff),
        "a plain quit is no handoff"
    );
}

fn assert_live(fixture: &Fixture, outcome: &AgentOutcome) {
    assert!(
        outcome
            .assistant_text()
            .iter()
            .any(|text| text.contains(PARKED))
    );
    assert!(
        invariants::dispatches(fixture).len() >= 2,
        "spawn and answer"
    );
    assert!(child_dispatches(fixture) >= 1);
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

/// The child's continuation commands the runtime accepted.
fn resumed(fixture: &Fixture, session: &str) -> usize {
    tree(fixture, session).map_or(0, |tree| {
        tree.rows()
            .commands()
            .values()
            .filter(|command| {
                command.recipient == "c1"
                    && command.command_id.starts_with(
                        plakous_recursive_subagents::tree::ProductAgentTree::LOSS_RESUME_COMMAND_PREFIX,
                    )
                    && command.fact == Some(plakous_recursive_subagents::tree::CommandFact::Accepted)
            })
            .count()
    })
}

/// Waits a bounded time for the child's requests to reach `count`.
fn wait_for_child_dispatches(fixture: &Fixture, count: usize) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while child_dispatches(fixture) < count && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// One cut's relaunches: a child the cut handed off continues once by
/// itself; under `continue`, so does one whose running request the cut
/// lost; nothing else continues. A second relaunch dispatches nothing.
pub(in super::super) fn after_kill(
    fixture: &Fixture,
    session: &str,
    commits: &[trace::Commit],
    variant: Variant,
    relaunch: impl Fn(&str) -> AgentProcess,
) -> Value {
    let before = before(fixture, session);
    let _ = std::fs::remove_file(fixture.root.join(super::handoff::QUIT_AS_HANDOFF));
    let root_before = invariants::dispatches(fixture).len();
    let child_before = child_dispatches(fixture);
    // The root answers, and a continued child waits again, so it is counted
    // while it runs and never asks for a second request.
    let _ = std::fs::remove_file(fixture.root.join(CHILD_RELEASE));
    write_child(fixture, NEVER);
    fixture.write_script(&root(
        fixture,
        &json!([{"output": {"text": FOLLOWUP}, "finish": "stop"}]),
    ));
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
    // A handed-off child continues by itself under either policy, and
    // nothing else does under `hold`. Under `continue` the rows before the
    // relaunch do not decide it: a request lost in flight is rejoined by
    // the custody take's recovery rather than continued, and the plain
    // quit's own release may have ended it in the runtime first. That
    // variant is bounded at once.
    let expected = match (before, variant) {
        (Before::HandedOff, _) => Some(1),
        (_, Variant::OwnerLost) => None,
        _ => Some(0),
    };
    wait_for_child_dispatches(fixture, child_before + expected.unwrap_or(1));
    // Long enough for a second continuation to show itself.
    std::thread::sleep(std::time::Duration::from_millis(500));
    reopened.send(&quit_frame());
    let first = reopened.finish();
    assert!(first.status.success(), "follow-up failed: {}", first.stderr);
    let child_after = child_dispatches(fixture);
    let continues = child_after - child_before;
    assert_eq!(
        continues,
        resumed(fixture, session),
        "{before:?}: each continuation is one consented command and one request"
    );
    assert!(
        expected.map_or(continues <= 1, |expected| continues == expected),
        "{before:?}: a lost or handed-off child continues once, and only by its rows; \
         it continued {continues} times"
    );
    let root_after = invariants::dispatches(fixture).len();
    assert_eq!(
        root_after,
        root_before + 1 + usize::from(requeued),
        "the root answers the follow-up, and a requeued prompt once"
    );
    invariants::no_duplicate_dispatch(fixture);

    let mut second = relaunch("relaunch-2");
    second.send(&quit_frame());
    assert!(second.finish().status.success());
    assert_eq!(
        child_dispatches(fixture),
        child_after,
        "second relaunch continued"
    );
    assert_eq!(resumed(fixture, session), continues);
    assert_eq!(invariants::dispatches(fixture).len(), root_after);
    json!({
        "outcome": match (before, continues) {
            (Before::HandedOff, _) => "HANDED_OFF",
            (_, 1) => "CONTINUED",
            (Before::Running, _) if variant == Variant::Handoff => "HELD",
            _ if requeued => "REQUEUED",
            _ => "NOT_HANDED_OFF",
        },
        "invariants": true,
        "dispatches": root_after,
        "child_dispatches": child_after,
        "global_order": commits,
        "global_ordinal": commits.last().unwrap().ordinal,
    })
}
