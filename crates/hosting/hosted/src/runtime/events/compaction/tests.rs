#![allow(clippy::unwrap_used, reason = "unit tests")]

use super::{compaction_event_outcome_detail, format_compact_tokens};

use crate::runtime::events::compaction::{
    compact_finished_note, compact_finished_record, compact_row_token_pair,
    compact_terminal_subject, compaction_rung_label,
};
use crate::session_home::SessionHome;
use crate::session_log::SessionLog;
use plakous::agent::{CompactionEventOutcome, CompactionRung, CompactionSummary};

#[test]
fn the_compact_row_subject_uses_occupancy_not_prepared_padding() {
    let occupancy =
        compact_terminal_subject(Some(44_900), Some(44_900), true, true).expect("occupancy sizes");
    assert_eq!(occupancy, "44.9k → 44.9k");
    let dropped =
        compact_terminal_subject(Some(395_000), Some(44_900), true, true).expect("overlay drop");
    assert_eq!(dropped, "395.0k → 44.9k");
    // The row states the successor's occupancy figures: the anchored
    // context measure before, the successor's `bytes÷4` after. The
    // prepared-input bound (395k → 223k) is the Context tab's admission
    // fact, not the row.
    let summary = CompactionSummary {
        rung: CompactionRung::ToolResultPlaceholders,
        segments: 1,
        input_tokens_before: 395_000,
        input_tokens_after: 222_700,
        occupancy_tokens_before: Some(138_600),
        occupancy_tokens_after: Some(44_900),
    };
    let (before, after) = compact_row_token_pair(Some(summary));
    let subject = compact_terminal_subject(before, after, true, true).expect("live overlay");
    assert_eq!(subject, "138.6k → 44.9k");
    let (prepared_before, prepared_after) = compact_row_token_pair(Some(CompactionSummary {
        occupancy_tokens_before: None,
        occupancy_tokens_after: None,
        ..summary
    }));
    assert_eq!(prepared_before, Some(395_000));
    assert_eq!(prepared_after, Some(222_700));
    assert_ne!(
        compact_terminal_subject(prepared_before, prepared_after, true, true).as_deref(),
        Some(subject.as_str()),
        "prepared-input padding is not Compact chrome when occupancy is known"
    );
}

#[test]
fn the_compact_row_subject_names_before_and_after_sizes() {
    let subject = compact_terminal_subject(Some(36_617), Some(852), true, true).expect("sizes");
    assert!(
        subject.contains('→') || subject.contains("36.6k"),
        "{subject}"
    );
    assert!(
        subject.contains("852") || subject.contains("0.9k"),
        "{subject}"
    );
}

/// A definite provider failure during compaction is diagnosable from the log.
///
/// Five `compact.started`/`compact.finished`
/// pairs, three committed and two failed, and the failed records carried
/// only `kind`, `outcome`, `elapsed_ms`, `seq` and `ts`. Nothing said what
/// had failed, so nothing could be acted on.
#[test]
fn a_failed_compaction_records_and_reports_why_it_failed() {
    let dir = tempfile::tempdir().unwrap();
    let home = SessionHome::new(dir.path().join("sessions"));
    home.ensure().unwrap();
    let id = "9f9f9f9f-1111-2222-3333-444444444444";
    let log = SessionLog::open(&home, id).unwrap();
    log.append(compact_finished_record(
        CompactionEventOutcome::Failed,
        4_210,
        None,
        Some("provider rejected the compaction request: context_capacity"),
    ))
    .unwrap();
    log.flush().unwrap();
    let lines = crate::session_log::read_tail(&home, id, 10).unwrap();
    let record: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(record["kind"], "compact.finished");
    assert_eq!(record["outcome"], "failed");
    assert_eq!(record["elapsed_ms"], 4_210);
    assert_eq!(
        record["detail"], "provider rejected the compaction request: context_capacity",
        "a failed compaction must say what failed"
    );

    let note = compact_finished_note(
        CompactionEventOutcome::Failed,
        None,
        Some("provider rejected the compaction request: context_capacity"),
    );
    assert!(
        note.contains("context_capacity"),
        "the operator note must carry the failure text too: {note}"
    );

    // An outcome with nothing to explain gains no empty field.
    log.append(compact_finished_record(
        CompactionEventOutcome::Committed,
        12,
        None,
        None,
    ))
    .unwrap();
    log.flush().unwrap();
    let lines = crate::session_log::read_tail(&home, id, 10).unwrap();
    assert!(
        !lines[1].contains("\"detail\""),
        "a committed compaction must not carry an empty detail: {}",
        lines[1]
    );
}

/// A committed compaction says what it did, not merely that it committed.
///
/// "compact workflow committed" tells an operator nothing about whether the
/// compaction bought anything, and the fold's note said only that for as
/// long as the fold has existed. The figures come from the successor
/// checkpoint's own compaction provenance, so the line and the record agree
/// with what the checkpoint commits to.
#[test]
fn a_committed_compaction_records_and_reports_what_it_did() {
    let dir = tempfile::tempdir().unwrap();
    let home = SessionHome::new(dir.path().join("sessions"));
    home.ensure().unwrap();
    let id = "7a7a7a7a-1111-2222-3333-444444444444";
    let log = SessionLog::open(&home, id).unwrap();
    let summary = CompactionSummary {
        rung: CompactionRung::SemanticFold,
        segments: 14,
        input_tokens_before: 182_400,
        input_tokens_after: 61_200,
        occupancy_tokens_before: None,
        occupancy_tokens_after: None,
    };
    log.append(compact_finished_record(
        CompactionEventOutcome::Committed,
        3_120,
        Some(summary),
        None,
    ))
    .unwrap();
    log.flush().unwrap();
    let lines = crate::session_log::read_tail(&home, id, 10).unwrap();
    let record: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(record["kind"], "compact.finished");
    assert_eq!(record["outcome"], "committed");
    assert_eq!(record["rung"], "semantic_fold");
    assert_eq!(record["segments"], 14);
    assert_eq!(record["input_tokens_before"], 182_400);
    assert_eq!(record["input_tokens_after"], 61_200);

    let note = compact_finished_note(CompactionEventOutcome::Committed, Some(summary), None);
    for figure in ["folded", "14", "182400", "61200"] {
        assert!(
            note.contains(figure),
            "the note must carry {figure}: {note}"
        );
    }
    assert!(
        !note.contains("compact workflow committed"),
        "the figures replace the bare wording rather than trailing it: {note}"
    );

    // Each rung is named distinctly, so a reader can tell which one ran.
    for (rung, label, verb) in [
        (CompactionRung::NativeFacet, "native_facet", "natively"),
        (CompactionRung::SemanticFold, "semantic_fold", "folded"),
        (
            CompactionRung::ToolResultPlaceholders,
            "tool_result_placeholders",
            "replaced",
        ),
    ] {
        assert_eq!(compaction_rung_label(rung), label);
        assert!(rung.verb().contains(verb), "{rung:?}");
    }
}

#[test]
fn compact_token_labels_round_to_tenths() {
    assert_eq!(format_compact_tokens(199_700), "199.7k");
    assert_eq!(format_compact_tokens(178_075), "178.1k");
    assert_eq!(format_compact_tokens(400), "400");
}

/// Spec section 9: every compaction outcome is distinct operator text,
/// including the ones the tool-group fence produces mid-turn, which were
/// logged and then dropped instead of reaching the transcript at all.
#[test]
fn every_compaction_outcome_has_its_own_operator_text() {
    let outcomes = [
        CompactionEventOutcome::Committed,
        CompactionEventOutcome::NotNeeded,
        CompactionEventOutcome::NoEligibleRange,
        CompactionEventOutcome::Unavailable,
        CompactionEventOutcome::Failed,
        CompactionEventOutcome::RecoveryRequired,
    ];
    let mut seen = std::collections::BTreeSet::new();
    for outcome in outcomes {
        let detail = compaction_event_outcome_detail(outcome);
        assert_ne!(detail, "");
        assert!(seen.insert(detail), "duplicate operator text: {detail}");
    }
    assert_eq!(seen.len(), outcomes.len());
}
