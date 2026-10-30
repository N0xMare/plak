//! Recovery status for a session and its children.

use crate::store::sessions as product_sessions;
use plakous::sessions::{
    AgentSessions, AgentSessionsErrorKind, RecoveryRequiredReport, SessionLocator,
};
use plakous_hosted::error::{CliFailure, CliResult};
use plakous_recursive_subagents_store::TreeInspection;
use std::io;
use std::path::Path;

/// Prints only canonical recovery identities and obligations. An out-of-date
/// product locator is displayed as recorded, then the exact current head is
/// inspected once; this never rewrites the product record or opens a runtime.
pub(crate) fn show_recovery_status(
    paths: &crate::home::paths::PlakPaths,
    record: &product_sessions::SessionRecord,
    stdout: &mut dyn io::Write,
) -> CliResult<()> {
    writeln!(stdout, "\nCanonical recovery (root session)")
        .map_err(|_| CliFailure::new("stdout failed"))?;
    let Some(recorded) = record.plakous_locator else {
        return writeln!(stdout, "  Recorded head  unavailable")
            .map_err(|_| CliFailure::new("stdout failed"));
    };
    writeln!(stdout, "  Recorded head  {recorded}")
        .map_err(|_| CliFailure::new("stdout failed"))?;
    let (current, recovery) = inspect_locator(paths, recorded)?;
    if current != recorded {
        writeln!(
            stdout,
            "  Current head   {current} (recorded head is stale; unchanged)"
        )
        .map_err(|_| CliFailure::new("stdout failed"))?;
    }
    match recovery {
        None => writeln!(stdout, "  Status         quiescent at {current}")
            .map_err(|_| CliFailure::new("stdout failed")),
        Some(report) => write_recovery_report(paths, stdout, &report),
    }
}

pub(super) fn inspect_locator(
    paths: &crate::home::paths::PlakPaths,
    recorded: SessionLocator,
) -> CliResult<(SessionLocator, Option<RecoveryRequiredReport>)> {
    Ok(
        match AgentSessions::inspect_recovery_sqlite(paths.runtime_database(), recorded) {
            Ok(recovery) => (recorded, recovery),
            Err(error) if error.kind() == AgentSessionsErrorKind::StaleLocator => {
                let current = error.current_locator().ok_or_else(|| {
                    CliFailure::new("stale canonical locator did not name the current head")
                })?;
                let recovery =
                    AgentSessions::inspect_recovery_sqlite(paths.runtime_database(), current)
                        .map_err(|error| {
                            CliFailure::new(format!(
                                "current canonical head changed or could not be inspected: {error}"
                            ))
                        })?;
                (current, recovery)
            }
            Err(error) => {
                return Err(CliFailure::new(format!(
                    "canonical recovery inspection failed: {error}"
                )));
            }
        },
    )
}

pub(super) fn write_recovery_report(
    paths: &crate::home::paths::PlakPaths,
    stdout: &mut dyn io::Write,
    report: &RecoveryRequiredReport,
) -> CliResult<()> {
    let status = working_status(paths, report.locator())?;
    writeln!(stdout, "  Status         {status}")
        .and_then(|()| writeln!(stdout, "  Working head   {}", report.locator()))
        .and_then(|()| writeln!(stdout, "  Phase          {:?}", report.phase()))
        .map_err(|_| CliFailure::new("stdout failed"))?;
    for obligation in report.obligations() {
        writeln!(
            stdout,
            "  Obligation     operation={} action={:?} request_identity={}",
            obligation.operation_id(),
            obligation.action(),
            obligation.request_identity()
        )
        .map_err(|_| CliFailure::new("stdout failed"))?;
    }
    Ok(())
}

fn working_status(
    paths: &crate::home::paths::PlakPaths,
    locator: SessionLocator,
) -> CliResult<&'static str> {
    let lock = paths
        .state_dir()
        .join("runtime.sqlite3-holders")
        .join(format!(
            "{}.{}.lock",
            locator.session_id(),
            locator.branch_id()
        ));
    Ok(if holder_is_live(&lock)? {
        "working (live owner present)"
    } else {
        "recovery required"
    })
}

/// Probe only an existing branch holder lock; never create a file or claim a lease.
fn holder_is_live(path: &Path) -> CliResult<bool> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(CliFailure::new(format!(
                "runtime holder inspection unavailable: {error}"
            )));
        }
    };
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockShared) {
        Ok(()) => Ok(false),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(true),
        Err(error) => Err(CliFailure::new(format!(
            "runtime holder inspection unavailable: {error}"
        ))),
    }
}

/// Inspects the root's persisted tree and each child locator. Tree status and
/// canonical checkpoint status are separate facts: a quiescent child checkpoint
/// does not establish that its node retired or that its effects are settled.
pub(crate) fn show_child_recovery_status(
    paths: &crate::home::paths::PlakPaths,
    record: &product_sessions::SessionRecord,
    stdout: &mut dyn io::Write,
) -> CliResult<()> {
    show_child_recovery_status_with(paths, record, stdout, |locator| {
        inspect_locator(paths, locator)
    })
}

fn show_child_recovery_status_with(
    paths: &crate::home::paths::PlakPaths,
    record: &product_sessions::SessionRecord,
    stdout: &mut dyn io::Write,
    mut inspect: impl FnMut(
        SessionLocator,
    ) -> CliResult<(SessionLocator, Option<RecoveryRequiredReport>)>,
) -> CliResult<()> {
    writeln!(stdout, "\nTopology children (durable snapshot)")
        .map_err(|_| CliFailure::new("stdout failed"))?;
    let (tree_id, observed) = crate::topology_product::inspect_session_tree(paths, record)?;
    match observed {
        TreeInspection::StoreAbsent => {
            writeln!(
                stdout,
                "  Tree store absent; child recovery was not inspected"
            )
            .map_err(|_| CliFailure::new("stdout failed"))?;
        }
        TreeInspection::TreeAbsent => {
            writeln!(
                stdout,
                "  No persisted tree at lineage head {tree_id}; child recovery was not inspected"
            )
            .map_err(|_| CliFailure::new("stdout failed"))?;
        }
        TreeInspection::Tree(tree) => {
            let tree = tree.rows();
            writeln!(stdout, "  Tree           {tree_id}")
                .map_err(|_| CliFailure::new("stdout failed"))?;
            for candidate in tree.candidates().values() {
                if let Some(commit) = &candidate.merged_at {
                    writeln!(
                        stdout,
                        "  Candidate      {} merged at {} (before {})",
                        crate::cli_output::safe_terminal_text(candidate.candidate_id.as_str(), 96),
                        crate::cli_output::safe_terminal_text(commit, 96),
                        crate::cli_output::safe_terminal_text(
                            candidate.target_before.as_deref().unwrap_or("unknown"),
                            96
                        )
                    )
                    .map_err(|_| CliFailure::new("stdout failed"))?;
                }
            }
            let mut count = 0_usize;
            let mut incomplete = false;
            for node in tree
                .nodes()
                .values()
                .filter(|node| node.parent_agent_id.is_some())
            {
                count += 1;
                let attempt = tree.attempt(&node.agent_id);
                let child_id = crate::cli_output::safe_terminal_text(&node.agent_id, 64);
                writeln!(
                    stdout,
                    "  Child          {} node={:?} attempt={}",
                    child_id,
                    node.status,
                    attempt.map_or_else(
                        || "unavailable".to_owned(),
                        |row| format!("{:?}", row.status)
                    )
                )
                .map_err(|_| CliFailure::new("stdout failed"))?;
                let Some(locator_text) = node.session_locator.as_deref() else {
                    writeln!(stdout, "    Canonical    locator unavailable")
                        .map_err(|_| CliFailure::new("stdout failed"))?;
                    incomplete = true;
                    continue;
                };
                let Ok(recorded) = locator_text.parse::<SessionLocator>() else {
                    writeln!(stdout, "    Canonical    invalid recorded locator")
                        .map_err(|_| CliFailure::new("stdout failed"))?;
                    incomplete = true;
                    continue;
                };
                writeln!(stdout, "    Recorded     {recorded}")
                    .map_err(|_| CliFailure::new("stdout failed"))?;
                let (current, recovery) = match inspect(recorded) {
                    Ok(result) => result,
                    Err(error) => {
                        writeln!(
                            stdout,
                            "    Canonical    inspection unavailable: {}",
                            crate::cli_output::safe_terminal_text(error.message(), 160)
                        )
                        .map_err(|_| CliFailure::new("stdout failed"))?;
                        incomplete = true;
                        continue;
                    }
                };
                if current != recorded {
                    writeln!(
                        stdout,
                        "    Current      {current} (recorded locator is stale; unchanged)"
                    )
                    .map_err(|_| CliFailure::new("stdout failed"))?;
                }
                match recovery {
                    None => writeln!(
                        stdout,
                        "    Canonical    quiescent at {current}; node retirement is separate"
                    )
                    .map_err(|_| CliFailure::new("stdout failed"))?,
                    Some(report) => {
                        let status = working_status(paths, report.locator())?;
                        writeln!(stdout, "    Canonical    {status}")
                            .and_then(|()| {
                                writeln!(stdout, "    Phase        {:?}", report.phase())
                            })
                            .map_err(|_| CliFailure::new("stdout failed"))?;
                        for obligation in report.obligations() {
                            writeln!(
                                stdout,
                                "    Obligation   operation={} action={:?} request_identity={}",
                                obligation.operation_id(),
                                obligation.action(),
                                obligation.request_identity()
                            )
                            .map_err(|_| CliFailure::new("stdout failed"))?;
                        }
                    }
                }
            }
            if count == 0 {
                writeln!(stdout, "  Children       none in this persisted tree")
                    .map_err(|_| CliFailure::new("stdout failed"))?;
            }
            if incomplete {
                return Err(CliFailure::recovery_required(
                    "one or more child recovery heads could not be inspected",
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod recovery_tests;
