use super::super::offline_support::{AgentOutcome, Fixture};
use super::summary::{self, Summary};
use plakous_offline_crash_harness::evidence::{EvidenceKind, read_evidence};
use std::collections::BTreeSet;

pub(super) fn dispatches(fixture: &Fixture) -> Vec<(String, String)> {
    let path = fixture.root.join("evidence.jsonl");
    if !path.exists() {
        return Vec::new();
    }
    read_evidence(path)
        .expect("physical evidence")
        .into_iter()
        .filter(|row| {
            matches!(
                row.kind,
                EvidenceKind::ProviderDispatched | EvidenceKind::EffectDispatched
            )
        })
        .map(|row| {
            (
                format!("{:?}", row.kind),
                row.operation_id.expect("physical operation id"),
            )
        })
        .collect()
}

pub(super) fn summary_dispatches(fixture: &Fixture) -> Vec<String> {
    read_evidence(fixture.root.join("evidence.summary.jsonl"))
        .expect("physical summary evidence")
        .into_iter()
        .map(|row| {
            assert_eq!(row.kind, EvidenceKind::ProviderDispatched);
            row.operation_id.expect("physical summary request identity")
        })
        .collect()
}

pub(super) fn summary_once(before: &[String], after: &[String], first_dispatch: usize) {
    assert!(
        before.len() <= 1 && after.len() <= 1,
        "duplicate physical summary dispatch across row"
    );
    assert!(after.starts_with(before));
    assert_eq!(
        after.len() - before.len(),
        first_dispatch,
        "runtime recovery did inference"
    );
}

#[test]
fn summary_total_is_bounded_across_both_relaunches() {
    let once = vec!["first-summary".to_owned()];
    summary_once(&[], &once, 1);
    summary_once(&once, &once, 0);
    let twice = vec![
        "first-summary".to_owned(),
        "new-physical-summary".to_owned(),
    ];
    assert!(std::panic::catch_unwind(|| summary_once(&once, &twice, 1)).is_err());
    assert!(std::panic::catch_unwind(|| summary_once(&[], &once, 0)).is_err());
    assert!(std::panic::catch_unwind(|| summary_once(&[], &twice, 2)).is_err());
}

pub(super) fn no_duplicate_dispatch(fixture: &Fixture) {
    let rows = dispatches(fixture);
    assert_eq!(
        rows.iter().collect::<BTreeSet<_>>().len(),
        rows.len(),
        "duplicate physical dispatch"
    );
}

pub(super) fn capacity_publication(state: &Summary, dispatch_count: usize) {
    assert!(state.capacity_terminals <= 1, "duplicate capacity terminal");
    assert!(state.queued_answers <= 1, "duplicate queued answer");
    if dispatch_count > 1 {
        assert_eq!(
            state.capacity_terminals, 1,
            "queued prompt dispatched past unpublished capacity terminal"
        );
    }
}

#[test]
fn capacity_publication_fences_queued_dispatch_and_duplicates() {
    let mut state = Summary::default();
    capacity_publication(&state, 1);
    assert!(std::panic::catch_unwind(|| capacity_publication(&state, 2)).is_err());
    state.capacity_terminals = 1;
    capacity_publication(&state, 2);
    state.capacity_terminals = 2;
    assert!(std::panic::catch_unwind(|| capacity_publication(&state, 2)).is_err());
    state.capacity_terminals = 1;
    state.queued_answers = 2;
    assert!(std::panic::catch_unwind(|| capacity_publication(&state, 2)).is_err());
}

pub(super) fn job_notices_once(before: &Summary, first: &AgentOutcome, second: &AgentOutcome) {
    let notices = first
        .records()
        .into_iter()
        .filter(|row| row["kind"] == "job_terminal")
        .collect::<Vec<_>>();
    let ids = notices
        .iter()
        .map(|row| row["job_id"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(ids.len(), notices.len(), "duplicate job terminal notice");
    let second_notices = second
        .records()
        .into_iter()
        .filter(|row| row["kind"] == "job_terminal")
        .collect::<Vec<_>>();
    let second_ids = second_notices
        .iter()
        .map(|row| row["job_id"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        second_ids.len(),
        second_notices.len(),
        "duplicate job snapshot within one attachment"
    );
    for job in &before.jobs {
        let id = format!("p{}", job["number"].as_u64().unwrap());
        let matches = notices
            .iter()
            .filter(|row| row["job_id"] == id)
            .collect::<Vec<_>>();
        assert_eq!(matches.len(), 1, "durable job notice lost: {id}");
        if job["terminal"] == false {
            assert_eq!(
                matches[0]["status"], "outcome_unknown",
                "invented lost-process outcome"
            );
        }
    }
}

pub(super) fn recovery_dispatches(
    _shape: &str,
    _before: &Summary,
    _recovered: &Summary,
    dispatched_before: &[(String, String)],
    dispatched_after: &[(String, String)],
) -> usize {
    assert_eq!(
        dispatched_after, dispatched_before,
        "recovery dispatched physical work"
    );
    0
}

#[test]
#[should_panic(expected = "recovery dispatched physical work")]
fn unadmitted_workflow_recovery_cannot_dispatch_a_summary() {
    recovery_dispatches(
        "scheduled_compaction_workflow",
        &Summary::default(),
        &Summary::default(),
        &[],
        &[("ProviderDispatched".into(), "summary".into())],
    );
}

pub(super) fn quiescent(state: &Summary) {
    assert_eq!(
        state.job_notices.iter().collect::<BTreeSet<_>>().len(),
        state.job_notices.len(),
        "duplicate canonical process notice receipt"
    );
    assert_eq!(
        state.locator.as_ref(),
        Some(&state.head),
        "wrong product locator"
    );
    assert!(state.phase.is_none(), "runtime remains Working");
    assert!(state.queue.is_empty(), "queue did not drain");
    assert!(
        state
            .submissions
            .iter()
            .all(|row| row["state"] == "accepted" || row["state"] == "rejected"),
        "submission remains pending"
    );
    assert!(
        state
            .workflow_requests
            .iter()
            .all(|row| row["state"] == "settled"),
        "workflow request remains pending"
    );
    assert!(
        state.original_answers <= 1,
        "duplicate original publication"
    );
    assert_eq!(
        state.followup_answers, 1,
        "follow-up publication missing or duplicated"
    );
    assert!(
        state.abandonment_claims.len() <= 1,
        "duplicate abandonment notice"
    );
    assert!(
        state.unapplied_steering_notices <= 1,
        "duplicate unapplied steering notice"
    );
}

pub(super) fn notices_once(fixture: &Fixture) {
    terminal_notices_once(summary::log_rows(fixture));
}

fn terminal_notices_once(rows: Vec<serde_json::Value>) {
    let mut usage = BTreeSet::new();
    let mut notices = BTreeSet::new();
    let mut turn = String::new();
    for row in rows {
        let kind = row["kind"].as_str().unwrap_or_default();
        if kind == "turn.lifecycle" {
            row["turn_id"].as_str().unwrap().clone_into(&mut turn);
        }
        if kind == "turn.usage" || kind == "turn.completed" {
            let terminal_turn = if kind == "turn.usage" {
                row["turn_id"]
                    .as_str()
                    .expect("terminal usage carries its authoritative turn id")
            } else {
                &turn
            };
            assert!(
                usage.insert((kind.to_owned(), terminal_turn.to_owned())),
                "duplicate terminal usage/turn notice"
            );
        }
        if kind == "job.notice" {
            assert!(
                notices.insert(row["job_id"].to_string()),
                "duplicate job notice"
            );
        }
    }
}

#[test]
fn catch_up_usage_is_not_attributed_to_the_previous_lifecycle() {
    terminal_notices_once(vec![
        serde_json::json!({"kind": "turn.lifecycle", "turn_id": "first"}),
        serde_json::json!({"kind": "turn.usage", "turn_id": "first"}),
        serde_json::json!({"kind": "turn.usage", "turn_id": "recovered"}),
    ]);
}

#[test]
#[should_panic(expected = "duplicate terminal usage/turn notice")]
fn duplicate_usage_is_rejected_even_after_another_lifecycle() {
    terminal_notices_once(vec![
        serde_json::json!({"kind": "turn.lifecycle", "turn_id": "first"}),
        serde_json::json!({"kind": "turn.usage", "turn_id": "first"}),
        serde_json::json!({"kind": "turn.lifecycle", "turn_id": "second"}),
        serde_json::json!({"kind": "turn.usage", "turn_id": "first"}),
    ]);
}
