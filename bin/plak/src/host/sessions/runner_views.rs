//! Neutral runner facts projected into the authenticated client vocabulary.

use plakous::runner::{PlacementFailure, ProbePhase, RunnerRow};
use plakous_host_protocol::wire;

pub(super) const fn phase(phase: ProbePhase) -> Option<wire::RunnerPhase> {
    Some(match phase {
        ProbePhase::Queued => return None,
        ProbePhase::Ssh => wire::RunnerPhase::Ssh,
        ProbePhase::Daemon => wire::RunnerPhase::Daemon,
        ProbePhase::Bind => wire::RunnerPhase::Bind,
        ProbePhase::Options => wire::RunnerPhase::Options,
        ProbePhase::Discovery => wire::RunnerPhase::Discovery,
        ProbePhase::Reconcile => wire::RunnerPhase::Reconcile,
        ProbePhase::LinkLost => wire::RunnerPhase::LinkLost,
    })
}

pub(in super::super) fn label(id: &str, rows: &[RunnerRow]) -> wire::RunnerLabel {
    wire::RunnerLabel {
        name: rows
            .iter()
            .find(|row| row.runner.runner_id == id)
            .and_then(|row| row.runner.names.first())
            .map_or_else(|| id.to_owned(), Clone::clone),
        runner_id: id.to_owned(),
    }
}

pub(in super::super) fn refusal(
    runner: &wire::RunnerLabel,
    failure: PlacementFailure,
    rows: &[RunnerRow],
) -> wire::RequestRefusal {
    match reason(failure, Some(runner), rows) {
        Ok(reason) => wire::RequestRefusal::PlacementRefused {
            runner: runner.clone(),
            reason: Box::new(reason),
        },
        Err(detail) => wire::RequestRefusal::SessionUnavailable { detail },
    }
}

fn reason(
    failure: PlacementFailure,
    requested: Option<&wire::RunnerLabel>,
    rows: &[RunnerRow],
) -> Result<wire::PlacementReason, String> {
    Ok(match failure {
        PlacementFailure::RunnerNotConfigured { saved_id } => {
            wire::PlacementReason::RunnerNotConfigured {
                saved_id,
                alias: requested.map(|runner| runner.name.clone()),
                current_id: requested
                    .and_then(|runner| {
                        rows.iter()
                            .find(|row| row.runner.names.contains(&runner.name))
                    })
                    .map(|row| row.runner.runner_id.clone()),
            }
        }
        PlacementFailure::RunnerNotPaired => wire::PlacementReason::RunnerNotPaired,
        PlacementFailure::RunnerIdentityMismatch { expected, actual } => {
            wire::PlacementReason::RunnerIdentityMismatch { expected, actual }
        }
        PlacementFailure::RunnerUnreachable {
            phase: failed,
            timed_out,
        } => wire::PlacementReason::RunnerUnreachable {
            phase: phase(failed).ok_or_else(|| {
                "runner probe reported a failure before pool admission".to_owned()
            })?,
            timed_out,
        },
        PlacementFailure::RunnerVersionMismatch { expected, actual } => {
            wire::PlacementReason::RunnerVersionMismatch {
                expected: wire::RunnerVersion {
                    protocol: expected.protocol,
                    revision: expected.source_revision,
                    flavor: expected.flavor,
                },
                actual: wire::RunnerVersion {
                    protocol: actual.protocol,
                    revision: actual.source_revision,
                    flavor: actual.flavor,
                },
            }
        }
        PlacementFailure::RunnerRootsOverlap {
            machine_id,
            conflicting_runner,
            path,
        } => wire::PlacementReason::RunnerRootsOverlap {
            machine_id,
            conflicting_runner,
            path,
        },
        PlacementFailure::WorkspaceRefused { reason } => {
            use plakous::runner::WorkspaceRefusal as Refusal;
            let (kind, canonical) = match reason {
                Refusal::InvalidPath => (wire::WorkspaceRefusalKind::InvalidPath, None),
                Refusal::NotCanonical { canonical } => {
                    (wire::WorkspaceRefusalKind::NotCanonical, Some(canonical))
                }
                Refusal::NotFound => (wire::WorkspaceRefusalKind::NotFound, None),
                Refusal::NotDirectory => (wire::WorkspaceRefusalKind::NotDirectory, None),
                Refusal::OutsideRoots => (wire::WorkspaceRefusalKind::OutsideRoots, None),
                Refusal::ProductHome => (wire::WorkspaceRefusalKind::ProductHome, None),
                Refusal::PermissionDenied => (wire::WorkspaceRefusalKind::PermissionDenied, None),
            };
            wire::PlacementReason::WorkspaceRefused { kind, canonical }
        }
    })
}

pub(super) fn status(id: &str, rows: &[RunnerRow]) -> Result<wire::RunnerStatus, String> {
    let Some(row) = rows.iter().find(|row| row.runner.runner_id == id) else {
        if id == "local" {
            return Ok(wire::RunnerStatus::Unknown);
        }
        return Ok(wire::RunnerStatus::Blocked {
            reason: wire::PlacementReason::RunnerNotConfigured {
                saved_id: id.to_owned(),
                alias: None,
                current_id: None,
            },
        });
    };
    Ok(match &row.status {
        plakous::runner::RunnerStatus::Unknown => wire::RunnerStatus::Unknown,
        plakous::runner::RunnerStatus::Ready => wire::RunnerStatus::Ready,
        plakous::runner::RunnerStatus::Blocked(failure) => wire::RunnerStatus::Blocked {
            reason: reason(failure.clone(), None, rows)?,
        },
    })
}

pub(super) fn options(
    row: &RunnerRow,
    rows: &[RunnerRow],
    recent_workspaces: Vec<wire::RecentWorkspace>,
) -> Result<wire::RunnerOptions, String> {
    Ok(wire::RunnerOptions {
        runner: label(&row.runner.runner_id, rows),
        aliases: row.runner.names.iter().skip(1).cloned().collect(),
        status: status(&row.runner.runner_id, rows)?,
        checked_at: row.checked_at,
        blocked_since: row.blocked_since,
        roots: row
            .options
            .as_ref()
            .and_then(|options| options.roots.clone()),
        default_workspace: row
            .options
            .as_ref()
            .and_then(|options| options.default_workspace.clone()),
        recent_workspaces,
        discovered_workspaces: row
            .discovery
            .workspaces
            .iter()
            .map(|workspace| wire::DiscoveredWorkspace {
                path: workspace.path.clone(),
                repo: workspace
                    .repo
                    .as_ref()
                    .map(|repo| wire::RepositoryMetadata {
                        name: repo.name.clone(),
                        commit: repo.commit.clone(),
                        head: match &repo.head {
                            plakous::runner::RepositoryHead::Branch { name } => {
                                wire::RepositoryHead::Branch { name: name.clone() }
                            }
                            plakous::runner::RepositoryHead::Detached => {
                                wire::RepositoryHead::Detached
                            }
                            plakous::runner::RepositoryHead::Unborn { name } => {
                                wire::RepositoryHead::Unborn { name: name.clone() }
                            }
                            plakous::runner::RepositoryHead::Unavailable => {
                                wire::RepositoryHead::Unavailable
                            }
                        },
                    }),
            })
            .collect(),
        discovery_truncated: row.discovery.truncated,
    })
}
