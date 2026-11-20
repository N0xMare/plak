use std::sync::Arc;

use crate::CliResult;

/// Uses the product's placement port in a real paired-runner fixture.
///
/// # Errors
/// A saved copy does not belong to its runner's recorded mirror.
pub fn scripted_tree_runner_port(
    roots: &crate::agent::ProductStateRoots,
    tree_id: &str,
    workspace: &std::path::Path,
    saved: Option<&crate::agent_tree::ProductAgentTree>,
) -> CliResult<Option<Arc<dyn crate::occupancy_vcs::VcsOps>>> {
    if roots.runner_registry().is_none() || roots.runner_services().is_none() {
        return Ok(None);
    }
    crate::agent::build::topology::tree_vcs(roots, tree_id, workspace, saved).map(Some)
}
