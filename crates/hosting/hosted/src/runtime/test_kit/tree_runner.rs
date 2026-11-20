use std::sync::Arc;

use crate::CliResult;

pub(super) fn attached<P: plakous_host_protocol::ProtocolExtension>(
    config: &super::HostConfig<P>,
    opened: &mut super::AttachedCodingSession,
    scripted_children: bool,
) {
    let tree_id = super::super::rebind::durable_tree_id(&config.topology_rows(), &config.session);
    opened.claim_registered_topology_owner(&tree_id);
    // Scripted bindings were compiled on a temporary executor. Attach runs
    // on the worker that owns the root and can actually service reconnects.
    if scripted_children
        && config.state_roots.runner_services().is_some()
        && config.state_roots.runner_registry().is_some()
        && let Some(runtime) = crate::topology_runtime::lookup(&tree_id)
    {
        crate::agent::placement::watch_debt(&runtime);
    }
}

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
