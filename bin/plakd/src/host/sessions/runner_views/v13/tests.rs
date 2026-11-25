use plakous::runner::{ConfiguredRunner, WorkspaceDiscovery};

use super::*;

fn runner_row(status: ProbeStatus) -> RunnerRow {
    RunnerRow {
        runner: ConfiguredRunner {
            runner_id: "runner-id".into(),
            names: vec!["paired".into(), "alias".into()],
        },
        status,
        checked_at: Some(11),
        blocked_since: Some(10),
        options: None,
        discovery: WorkspaceDiscovery::default(),
    }
}

fn picker_row(rows: &[RunnerRow]) -> RunnerOptions {
    let row = &rows[0];
    let shared =
        super::super::options(row, rows, Vec::new()).unwrap_or_else(|error| panic!("{error}"));
    options(shared, row, rows)
}

#[test]
fn reconciliation_transitions_share_the_typed_phase_across_client_projections() {
    use ReconcileSubphase as Step;
    let steps = [
        (ProbePhase::ReconcileStore, Step::Store),
        (ProbePhase::ReconcileObsoleteLines, Step::ObsoleteLines),
        (ProbePhase::ReconcileTerminals, Step::Terminals),
        (ProbePhase::ReconcileJobs, Step::Jobs),
        (ProbePhase::ReconcileStops, Step::Stops),
        (ProbePhase::ReconcileForeground, Step::Foreground),
    ];
    for (probe, subphase) in steps {
        let phase = RunnerPhase::Reconcile {
            subphase: Some(subphase),
        };
        let timed_out = probe == ProbePhase::ReconcileJobs;
        let failure = PlacementFailure::RunnerUnreachable {
            phase: probe,
            timed_out,
        };
        let rows = [runner_row(ProbeStatus::Blocked(failure.clone()))];
        let runner = super::super::label("runner-id", &rows);
        let expected =
            PlacementReason::Draft(PlacementReasonV13::RunnerUnreachable { phase, timed_out });
        let blocked = RunnerStatus::Blocked {
            reason: expected.clone(),
        };

        assert_eq!(status(&runner.runner_id, &rows), blocked);
        assert_eq!(
            options_progress(1, runner.clone(), probe, None),
            Some(HostFrameV13::OptionsProgress {
                request_id: 1,
                runner: runner.clone(),
                phase: Some(phase),
                row: None,
            })
        );
        let picker = picker_row(&rows);
        assert_eq!(picker.status, blocked);
        assert_eq!(picker.checked_at, Some(11));
        assert_eq!(picker.blocked_since, Some(10));
        assert_eq!(picker.aliases, ["alias"]);
        assert_eq!(
            refusal(&runner, failure, &rows),
            RequestRefusal::Draft(RequestRefusalV13::PlacementRefused {
                runner,
                reason: Box::new(expected),
            })
        );
    }
}

#[test]
fn broad_markers_do_not_invent_a_subphase_and_ready_clears_the_failure() {
    let broad = PlacementFailure::RunnerUnreachable {
        phase: ProbePhase::Reconcile,
        timed_out: true,
    };
    let blocked = [runner_row(ProbeStatus::Blocked(broad.clone()))];
    let runner = super::super::label("runner-id", &blocked);
    assert_eq!(status("runner-id", &blocked), RunnerStatus::Unknown);
    assert!(options_progress(1, runner.clone(), ProbePhase::Reconcile, None).is_none());
    assert_eq!(
        refusal(&runner, broad, &blocked),
        RequestRefusal::session_unavailable("runner paired: reconciliation timed out".into()),
    );

    let mut ready = runner_row(ProbeStatus::Ready);
    ready.blocked_since = None;
    let rows = [ready];
    assert_eq!(status("runner-id", &rows), RunnerStatus::Ready);
    let picker = picker_row(&rows);
    assert_eq!(picker.status, RunnerStatus::Ready);
    assert_eq!(picker.blocked_since, None);
    assert_eq!(
        options_progress(
            1,
            runner.clone(),
            ProbePhase::Reconcile,
            Some(picker.clone())
        ),
        Some(HostFrameV13::OptionsProgress {
            request_id: 1,
            runner: runner.clone(),
            phase: None,
            row: Some(Box::new(picker)),
        }),
    );
    assert!(matches!(
        options_progress(1, runner, ProbePhase::Queued, None),
        Some(HostFrameV13::OptionsProgress {
            phase: None,
            row: None,
            ..
        })
    ));
    assert!(matches!(
        status("not-configured", &rows),
        RunnerStatus::Blocked { reason: PlacementReason::Shared(reason) }
            if matches!(reason.as_inner(), wire::PlacementReason::RunnerNotConfigured { saved_id, .. }
                if saved_id == "not-configured")
    ));
}
