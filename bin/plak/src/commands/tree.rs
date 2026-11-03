//! `plak tree release --force <tree-id>`: the operator's custody break-glass
//! (spec 03 §14, spec 02 §3).
//!
//! A tree's custody holder on this machine that has exited is taken over by
//! the next attach without this command. This is for a holder on another
//! machine, or one that cannot be proven gone; a holder proven to be running
//! here is refused, since forcing it would only make it fail its next write.

use std::io::Write;

use plakous_hosted::agent_tree::{CustodyRecord, TreeCustody as _, holder_runs_here};
use plakous_hosted::{CliFailure, CliFailureKind, CliResult};

use crate::home::paths::PlakPaths;

/// Releases `tree_id`'s custody record whoever holds it, after the caller
/// confirmed with `--force`.
///
/// # Errors
///
/// A holder proven to be running on this machine, and an unreachable store.
pub(crate) fn release(
    paths: &PlakPaths,
    tree_id: &str,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> CliResult<()> {
    let store = crate::store::tree::open_tree_store(paths).map_err(CliFailure::new)?;
    let written =
        |result: std::io::Result<()>| result.map_err(|_| CliFailure::new("stdout failed"));
    let Some(record) = store
        .custody(tree_id)
        .map_err(|error| CliFailure::new(error.detail))?
    else {
        return written(writeln!(
            stdout,
            "agent tree {tree_id} has no custody record; nothing to release"
        ));
    };
    if record.released {
        return written(writeln!(
            stdout,
            "custody of agent tree {tree_id} is already released (fence {}); nothing to release",
            record.fence
        ));
    }
    let holder = &record.holding.host;
    if holder_runs_here(&store, holder) {
        return Err(CliFailure::classified(
            CliFailureKind::HostedElsewhere,
            format!(
                "tree_hosted_elsewhere ({}): agent tree {tree_id} is held by a plak process still running on this machine (instance {}); quit it instead of forcing its release",
                holder.binary.as_deref().unwrap_or("unknown build"),
                holder.instance
            ),
        ));
    }
    let _ = writeln!(
        stderr,
        "plak: warning: forcing the release of agent tree {tree_id} from {} on {} (instance {}, build {}) without its holder; if that process is still running, every agent tree write it makes from now on is refused",
        holder.host_id,
        holder.host_name,
        holder.instance,
        holder.binary.as_deref().unwrap_or("unknown")
    );
    let released = store
        .break_custody(tree_id)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    written(writeln!(
        stdout,
        "{}",
        released_line(tree_id, released.as_ref())
    ))
}

fn released_line(tree_id: &str, released: Option<&CustodyRecord>) -> String {
    released.map_or_else(
        || format!("custody of agent tree {tree_id} was released meanwhile; nothing to release"),
        |record| {
            format!(
                "released custody of agent tree {tree_id} (fence {}, last held by {} on {}); the next attach takes it",
                record.fence, record.holding.host.host_id, record.holding.host.host_name
            )
        },
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
