use plakd_wire_types::v13::{
    HostFrameV13, PlacementReason, PlacementReasonV13, ReconcileSubphase, RequestRefusal,
    RequestRefusalV13, RunnerOptions, RunnerPhase, RunnerStatus,
};
use plakous_host_protocol::wire::{self, RunnerLabel};

use super::{PlacementFailure, ProbePhase, ProbeStatus, RunnerRow};

pub(in super::super) const fn phase(probe: ProbePhase) -> Option<RunnerPhase> {
    use ReconcileSubphase as Step;
    Some(match probe {
        ProbePhase::Queued | ProbePhase::Reconcile => return None,
        ProbePhase::Ssh => RunnerPhase::Ssh,
        ProbePhase::Daemon => RunnerPhase::Daemon,
        ProbePhase::Bind => RunnerPhase::Bind,
        ProbePhase::Options => RunnerPhase::Options,
        ProbePhase::Discovery => RunnerPhase::Discovery,
        ProbePhase::LinkLost => RunnerPhase::LinkLost,
        ProbePhase::ReconcileStore => RunnerPhase::Reconcile {
            subphase: Step::Store,
        },
        ProbePhase::ReconcileObsoleteLines => RunnerPhase::Reconcile {
            subphase: Step::ObsoleteLines,
        },
        ProbePhase::ReconcileTerminals => RunnerPhase::Reconcile {
            subphase: Step::Terminals,
        },
        ProbePhase::ReconcileJobs => RunnerPhase::Reconcile {
            subphase: Step::Jobs,
        },
        ProbePhase::ReconcileStops => RunnerPhase::Reconcile {
            subphase: Step::Stops,
        },
        ProbePhase::ReconcileForeground => RunnerPhase::Reconcile {
            subphase: Step::Foreground,
        },
    })
}

fn placement_reason(
    failure: PlacementFailure,
    requested: Option<&RunnerLabel>,
    rows: &[RunnerRow],
) -> Option<PlacementReason> {
    if let PlacementFailure::RunnerUnreachable {
        phase: failed,
        timed_out,
    } = failure
    {
        return Some(PlacementReason::Draft(
            PlacementReasonV13::RunnerUnreachable {
                phase: phase(failed)?,
                timed_out,
            },
        ));
    }
    super::reason(failure, requested, rows)
        .ok()?
        .try_into()
        .ok()
}

pub(in super::super) fn refusal(
    runner: &RunnerLabel,
    failure: PlacementFailure,
    rows: &[RunnerRow],
) -> RequestRefusal {
    if let PlacementFailure::RunnerUnreachable {
        phase: ProbePhase::Reconcile,
        timed_out,
    } = failure
    {
        let outcome = if timed_out {
            "timed out"
        } else {
            "unavailable"
        };
        return RequestRefusal::session_unavailable(format!(
            "runner {}: reconciliation {outcome}",
            runner.name
        ));
    }
    match placement_reason(failure, Some(runner), rows) {
        Some(reason) => RequestRefusal::Draft(RequestRefusalV13::PlacementRefused {
            runner: runner.clone(),
            reason: Box::new(reason),
        }),
        None => RequestRefusal::session_unavailable(
            "runner probe reported a failure before pool admission".to_owned(),
        ),
    }
}

pub(in super::super) fn status(id: &str, rows: &[RunnerRow]) -> RunnerStatus {
    match rows.iter().find(|row| row.runner.runner_id == id) {
        Some(row) => row_status(row, rows),
        None if id == "local" => RunnerStatus::Unknown,
        None => blocked(
            PlacementFailure::RunnerNotConfigured {
                saved_id: id.to_owned(),
            },
            rows,
        ),
    }
}

fn row_status(row: &RunnerRow, rows: &[RunnerRow]) -> RunnerStatus {
    match &row.status {
        ProbeStatus::Ready => RunnerStatus::Ready,
        ProbeStatus::Unknown => RunnerStatus::Unknown,
        ProbeStatus::Blocked(failure) => blocked(failure.clone(), rows),
    }
}

fn blocked(failure: PlacementFailure, rows: &[RunnerRow]) -> RunnerStatus {
    // An outer barrier marker has no subphase. Do not attribute a failure to
    // any of its operations until the probe reports which one it reached.
    placement_reason(failure, None, rows).map_or(RunnerStatus::Unknown, |reason| {
        RunnerStatus::Blocked { reason }
    })
}

pub(in super::super) fn options(
    shared: wire::RunnerOptions,
    row: &RunnerRow,
    rows: &[RunnerRow],
) -> RunnerOptions {
    RunnerOptions {
        runner: shared.runner,
        aliases: shared.aliases,
        status: row_status(row, rows),
        checked_at: shared.checked_at,
        blocked_since: shared.blocked_since,
        roots: shared.roots,
        default_workspace: shared.default_workspace,
        recent_workspaces: shared.recent_workspaces,
        discovered_workspaces: shared.discovered_workspaces,
        discovery_truncated: shared.discovery_truncated,
    }
}

pub(in super::super) fn options_progress(
    request_id: wire::RequestId,
    runner: RunnerLabel,
    probe: ProbePhase,
    row: Option<RunnerOptions>,
) -> Option<HostFrameV13> {
    if probe == ProbePhase::Reconcile && row.is_none() {
        return None;
    }
    Some(HostFrameV13::OptionsProgress {
        request_id,
        runner,
        phase: phase(probe),
        row: row.map(Box::new),
    })
}

#[cfg(test)]
mod tests;
