//! `handoff_resume` (spec 03 §14.1): a running root turn whose host quits
//! as a planned handoff. Every cut kills the host at one of its commits. A
//! cut after the handoff row is durable relaunches into one continuation of
//! the turn, by itself; any earlier cut relaunches into nothing continued.
//! A second relaunch continues nothing more.

use super::super::super::offline_support::{
    AgentOutcome, AgentProcess, Fixture, prompt_frame, quit_frame,
};
use super::super::{invariants, summary, trace};
use super::{FOLLOWUP, Lifecycle, Value};
use plak::store::session_records::SessionRecordStore as _;
use plakous_hosted::sessions::projector::RootInterruptCause;
use serde_json::json;

/// The handed-off turn's streamed prefix.
const PREFIX: &str = "crash-sweep-handoff-prefix";

/// The file beside the evidence log that makes the face's quit a planned
/// handoff.
pub(in super::super) const QUIT_AS_HANDOFF: &str = "quit-as-handoff";

pub(super) fn script() -> Value {
    json!({
        "version": 1,
        "steps": [{"stream": [{"text": PREFIX}, {"stall": 30000}]}]
    })
}

/// Prompts, then quits as a handoff while the turn streams its prefix.
pub(super) fn lifecycle(_: &Fixture, records: &[Value], state: &mut Lifecycle) -> Option<Value> {
    if !state.sent && records.iter().any(|row| row["kind"] == "ready") {
        state.sent = true;
        return Some(prompt_frame("crash-sweep-original-prompt"));
    }
    if state.sent
        && !state.quit
        && records.iter().any(|row| {
            row["kind"] == "assistant"
                && row["text"]
                    .as_str()
                    .is_some_and(|text| text.contains(PREFIX))
        })
    {
        state.quit = true;
        return Some(quit_frame());
    }
    None
}

fn root_interrupts(fixture: &Fixture, session: &str) -> Vec<RootInterruptCause> {
    let store = plak::store::session_records::SqliteSessionStore::new(summary::paths(fixture));
    store
        .load_intents(session)
        .unwrap()
        .root_interrupts
        .iter()
        .map(|intent| intent.value.cause)
        .collect()
}

fn root_continuations(fixture: &Fixture, session: &str) -> usize {
    let store = plak::store::session_records::SqliteSessionStore::new(summary::paths(fixture));
    store
        .load_intents(session)
        .unwrap()
        .root_continuations
        .len()
}

/// The live handoff recorded the running turn as handed off and dispatched
/// only the original request.
pub(super) fn assert_live(fixture: &Fixture, outcome: &AgentOutcome, _: &summary::Summary) {
    assert!(
        outcome
            .assistant_text()
            .iter()
            .any(|text| text.contains(PREFIX))
    );
    let paths = summary::paths(fixture);
    let store = plak::store::session_records::SqliteSessionStore::new(paths);
    let record = store
        .list_for_workspace(&fixture.workspace())
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        root_interrupts(fixture, &record.id),
        [RootInterruptCause::Handoff],
        "the handoff is recorded before the release"
    );
    assert_eq!(invariants::dispatches(fixture).len(), 1);
}

/// One cut's relaunches: the handed-off turn continues once by itself
/// (before the follow-up prompt the relaunch then answers), or, with no
/// durable handoff row, nothing continues; an original prompt the cut left
/// queued runs once on `resume_queue`. A second relaunch dispatches
/// nothing.
pub(in super::super) fn after_kill(
    fixture: &Fixture,
    session: &str,
    commits: &[trace::Commit],
    relaunch: impl Fn(&str) -> AgentProcess,
) -> Value {
    let handed_off = root_interrupts(fixture, session).contains(&RootInterruptCause::Handoff);
    let before = invariants::dispatches(fixture).len();
    let _ = std::fs::remove_file(fixture.root.join(QUIT_AS_HANDOFF));
    fixture.write_script(&json!({
        "version": 1,
        "repeat_last_step": true,
        "steps": [{"output": {"text": FOLLOWUP}, "finish": "stop"}]
    }));
    let mut reopened = relaunch("relaunch-1");
    // The open restores an original prompt the cut left unadmitted.
    let restored = reopened.wait_for("the restored queue", |row| row["kind"] == "queue");
    let requeued = restored["items"]
        .as_array()
        .is_some_and(|items| !items.is_empty());
    assert!(!(handed_off && requeued), "a handed-off turn was admitted");
    let earlier = usize::from(handed_off || requeued);
    if requeued {
        reopened.send(&json!({"v": 1, "kind": "resume_queue"}));
    }
    if earlier == 1 {
        reopened.wait_for_completed_turns(1);
    }
    reopened.send(&prompt_frame("crash-sweep-followup-prompt"));
    reopened.wait_for_completed_turns(1 + earlier);
    reopened.send(&quit_frame());
    let first = reopened.finish();
    assert!(first.status.success(), "follow-up failed: {}", first.stderr);
    let after = invariants::dispatches(fixture).len();
    assert_eq!(
        after,
        before + 1 + earlier,
        "a handoff continues once, and only with a durable handoff row"
    );
    assert_eq!(
        root_continuations(fixture, session),
        usize::from(handed_off)
    );
    invariants::no_duplicate_dispatch(fixture);

    let mut second = relaunch("relaunch-2");
    second.send(&quit_frame());
    assert!(second.finish().status.success());
    assert_eq!(
        invariants::dispatches(fixture).len(),
        after,
        "second relaunch dispatched"
    );
    assert_eq!(
        root_continuations(fixture, session),
        usize::from(handed_off)
    );
    json!({
        "outcome": if handed_off { "HANDED_OFF" } else if requeued { "REQUEUED" } else { "NOT_HANDED_OFF" },
        "invariants": true,
        "dispatches": after,
        "global_order": commits,
        "global_ordinal": commits.last().unwrap().ordinal,
    })
}
