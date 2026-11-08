//! The session tree's children, looked up on the process's topology hub.

use crate::agent_tree_protocol::NodeStatus;

use super::{ChildNodes, NodeState};

pub(super) fn child_counts(tree_id: &str) -> (usize, usize) {
    crate::topology_runtime::lookup(tree_id)
        .and_then(|runtime| runtime.read(count_children).ok())
        .unwrap_or_default()
}

pub(super) fn lifecycle_children(tree_id: &str) -> Result<Vec<String>, ()> {
    let Some(runtime) = crate::topology_runtime::lookup(tree_id) else {
        return Ok(Vec::new());
    };
    runtime
        .read(|tree| {
            tree.nodes()
                .iter()
                .filter(|(id, node)| {
                    **id != tree.root_agent_id
                        && matches!(
                            node.status,
                            NodeStatus::Pending
                                | NodeStatus::Running
                                | NodeStatus::Paused
                                | NodeStatus::Sealing
                        )
                })
                .map(|(id, _)| id.clone())
                .collect()
        })
        .map_err(|_| ())
}

fn count_children(tree: &crate::agent_tree::ProductAgentTree) -> (usize, usize) {
    use crate::agent_tree::LossContinuationState;
    let mut running = 0;
    let mut held = 0;
    for (id, node) in tree.nodes() {
        if *id == tree.root_agent_id {
            continue;
        }
        if tree
            .loss_continuation(id)
            .is_some_and(|continuation| continuation.state == LossContinuationState::Held)
        {
            held += 1;
        } else if matches!(
            node.status,
            NodeStatus::Pending | NodeStatus::Running | NodeStatus::Paused | NodeStatus::Sealing
        ) {
            running += 1;
        }
    }
    (running, held)
}

/// The children of the tree `tree_id`, as its hub holds them: a node of a
/// hub that is shutting down is retiring, a closed node is closed, a node
/// the tree's root may not write to is out of the operator's reach, and a
/// node the tree does not hold (or a tree no hub holds) is unknown.
pub struct TreeNodes {
    tree_id: String,
}

impl TreeNodes {
    /// The children of the tree `tree_id`.
    #[must_use]
    pub fn new(tree_id: impl Into<String>) -> Self {
        Self {
            tree_id: tree_id.into(),
        }
    }
}

impl ChildNodes for TreeNodes {
    fn session_info(
        &self,
        agent_id: &str,
    ) -> Option<plakous_host_protocol::views::SessionInfoView> {
        let runtime = crate::topology_runtime::lookup(&self.tree_id)?;
        runtime
            .read(|tree| {
                let node = crate::topology_navigator::project_snapshot(tree)
                    .nodes
                    .into_iter()
                    .find(|node| node.agent_id == agent_id)?;
                Some(plakous_host_protocol::views::SessionInfoView {
                    runner: node.runner,
                    host_version: plakous::VERSION.to_owned(),
                    working_dir: node.workspace_path,
                    root_model: Some(node.model_tuple),
                    root_effort: node.model_effort,
                    ..Default::default()
                })
            })
            .ok()
            .flatten()
    }

    fn state(&self, agent_id: &str) -> NodeState {
        let Some(runtime) = crate::topology_runtime::lookup(&self.tree_id) else {
            return NodeState::Unknown;
        };
        if runtime.is_closed() {
            return NodeState::Retiring;
        }
        runtime
            .read(|tree| {
                if agent_id == tree.root_agent_id {
                    return NodeState::Unknown;
                }
                let reached = tree
                    .grants()
                    .get(&tree.root_agent_id)
                    .is_some_and(|grant| grant.message_peers.iter().any(|peer| peer == agent_id));
                match tree.node(agent_id) {
                    Ok(node) if node.status == NodeStatus::Closed => NodeState::Closed,
                    Ok(_) if !reached => NodeState::OutOfReach,
                    Ok(_) => NodeState::Open,
                    Err(_) => NodeState::Unknown,
                }
            })
            .unwrap_or(NodeState::Unknown)
    }

    fn interrupt(&self, agent_id: &str, done: super::ControlDone) {
        let Some(runtime) = crate::topology_runtime::lookup(&self.tree_id) else {
            done(Err("no agent tree is attached".to_owned()));
            return;
        };
        let Some(executor) = runtime.executor() else {
            done(Err("the agent tree has no executor".to_owned()));
            return;
        };
        let agent = agent_id.to_owned();
        let done = Answer(Some(done));
        drop(executor.spawn(async move {
            done.send(crate::topology_navigator::interrupt_child(&runtime, &agent).await);
        }));
    }
}

/// A control's `done`, answered once: dropped unanswered (the executor
/// stopped before or while the control ran), it answers that.
struct Answer(Option<super::ControlDone>);

impl Answer {
    fn send(mut self, result: Result<(), String>) {
        if let Some(done) = self.0.take() {
            done(result);
        }
    }
}

impl Drop for Answer {
    fn drop(&mut self) {
        if let Some(done) = self.0.take() {
            done(Err("the agent tree's executor stopped".to_owned()));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
