#![allow(clippy::unwrap_used, reason = "unit tests")]

use crate::runtime::events::notes::{
    abandoned_turn_operator_detail, observation_clipped_note, observation_replaced_record,
    output_bound_rederived_note,
};
use crate::session_home::SessionHome;
use crate::session_log::{SessionEvent, SessionLog};

#[test]
fn completed_round_recovery_notice_does_not_invent_tool_results() {
    let line = abandoned_turn_operator_detail(
        plakous::agent::AbandonedTurnClaim::ToolRoundCompleted,
        plakous::agent::AbandonmentCause::ProcessLost,
        true,
        true,
    );
    assert!(line.contains("last round completed"));
    assert!(line.contains("before the next model request"));
    assert!(line.contains("completed work was kept"));
    assert!(!line.contains("tool results"));
}

#[test]
fn a2_real_tool_round_keeps_original_operator_line() {
    assert_eq!(
        abandoned_turn_operator_detail(
            plakous::agent::AbandonedTurnClaim::ToolRoundCompleted,
            plakous::agent::AbandonmentCause::ProcessLost,
            true,
            false
        ),
        "plak: recovered an interrupted turn; its tool round completed, but the turn was interrupted before the next model request; completed tool results were kept"
    );
}

#[test]
fn recovery_review_operator_line_distinguishes_ledger_from_conversation() {
    for cause in [
        plakous::agent::AbandonmentCause::ProcessLost,
        plakous::agent::AbandonmentCause::Interrupted,
    ] {
        assert_eq!(
            abandoned_turn_operator_detail(
                plakous::agent::AbandonedTurnClaim::RunningTool,
                cause,
                false,
                false
            ),
            "plak: recovered an interrupted turn; a tool may have changed this workspace; \
                 its result was not added to the conversation and it is not retried - check \
                 the workspace before carrying on"
        );
    }
}

/// A reopen whose route moved says both figures and the one way to the new one.
///
/// The session keeps the budget it was created with, because that figure is
/// identity-bearing and the persisted artefacts cannot be re-signed. So an
/// old session on a route that has since grown is starved, and the line has
/// to say so: without the larger figure and the instruction, the operator
/// has no way to learn the budget exists or how to reach it.
#[test]
#[allow(
    unused_qualifications,
    reason = "the test module has no NonZeroU32 import of its own"
)]
fn the_rederived_bound_note_names_both_figures() {
    let previous = std::num::NonZeroU32::new(4_096);
    let current = std::num::NonZeroU32::new(32_768);
    let note = output_bound_rederived_note(previous, current);
    assert!(note.contains("keeps its output budget"), "{note}");
    assert!(note.contains("4096"), "{note}");
    assert!(note.contains("32768"), "{note}");
    assert!(note.contains("start a new session"), "{note}");
    // A session created with no bound at all reads honestly, not as zero.
    let unbounded = output_bound_rederived_note(None, current);
    assert!(unbounded.contains("none"), "{unbounded}");
    assert!(unbounded.contains("32768"), "{unbounded}");
}

/// A truncated tool result is visible without reading the model's prose.
///
/// Until this existed the observation budget clipped a result silently: a
/// surface saw the tool succeed and had no way to know its output had been
/// cut, so the only trace was whatever the model said about it.
#[test]
fn a_clipped_observation_records_and_reports_what_was_cut() {
    let dir = tempfile::tempdir().unwrap();
    let home = SessionHome::new(dir.path().join("sessions"));
    home.ensure().unwrap();
    let id = "5c5c5c5c-1111-2222-3333-444444444444";
    let log = SessionLog::open(&home, id).unwrap();
    log.append(
        SessionEvent::new("observation.clipped")
            .str("name", "read")
            .opt_str("call_id", Some("call-9"))
            .u64("bytes_kept", 4_096)
            .u64("bytes_dropped", 118_784)
            .u64("bound", 4_096),
    )
    .unwrap();
    log.flush().unwrap();
    let lines = crate::session_log::read_tail(&home, id, 10).unwrap();
    let record: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(record["kind"], "observation.clipped");
    assert_eq!(record["name"], "read");
    assert_eq!(record["call_id"], "call-9");
    assert_eq!(record["bytes_kept"], 4_096);
    assert_eq!(record["bytes_dropped"], 118_784);
    assert_eq!(record["bound"], 4_096);

    let note = observation_clipped_note("read", 4_096, 118_784, 4_096);
    for figure in ["read", "4096", "118784"] {
        assert!(
            note.contains(figure),
            "the note must carry {figure}: {note}"
        );
    }
    assert!(
        note.contains("narrow the call"),
        "the note must say what to do about it: {note}"
    );
}

#[test]
fn a_replaced_observation_records_its_distinct_canonical_counts() {
    let dir = tempfile::tempdir().unwrap();
    let home = SessionHome::new(dir.path().join("sessions"));
    home.ensure().unwrap();
    let id = "5c5c5c5c-1111-2222-3333-444444444445";
    let log = SessionLog::open(&home, id).unwrap();
    log.append(observation_replaced_record(Some("call-10"), 4_157, 161))
        .unwrap();
    log.flush().unwrap();
    let lines = crate::session_log::read_tail(&home, id, 10).unwrap();
    let record: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(record["kind"], "observation.replaced");
    assert_eq!(record["call_id"], "call-10");
    assert_eq!(record["bytes_before"], 4_157);
    assert_eq!(record["bytes_after"], 161);
    assert!(record.get("bytes_dropped").is_none());
}
