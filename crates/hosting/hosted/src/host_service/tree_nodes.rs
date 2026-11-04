//! The session tree's children, looked up on the process's topology hub.

use crate::agent_tree_protocol::NodeStatus;

use super::{ChildNodes, NodeState};

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
