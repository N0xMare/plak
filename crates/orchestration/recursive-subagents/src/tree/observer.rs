//! The tree's telemetry port.
//!
//! The tree reports three facts about itself — a node was spawned, a node's
//! session ended, an epoch advanced — and nothing else. Every report is
//! content-free by construction: ids, a template name, and one status, never a
//! prompt, a path, a body, or a usage number. What the host does with them
//! (which telemetry vocabulary, which store, whether to record at all) is the
//! host's, and the tree cannot see it.

use super::NodeStatus;

/// Why a node's session ended, as the tree saw it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeDetach {
    /// The node submitted and finished.
    Completed,
    /// The attempt failed.
    Failed,
    /// An ancestor closed it.
    Closed,
    /// It reopened mid-flight and needs an operator.
    RecoveryRequired,
    /// The runtime went down under it.
    Shutdown,
}

impl NodeDetach {
    /// The detach a terminal node status implies; `None` while the node is
    /// still live, which has no detach to report yet.
    #[must_use]
    pub fn from_node_status(status: NodeStatus) -> Option<Self> {
        match status {
            NodeStatus::Completed => Some(Self::Completed),
            NodeStatus::Failed => Some(Self::Failed),
            NodeStatus::Closed => Some(Self::Closed),
            NodeStatus::RecoveryRequired => Some(Self::RecoveryRequired),
            _ => None,
        }
    }
}

/// What the tree tells its host. Optional everywhere it is held: a tree with
/// no observer runs identically and reports nothing.
pub trait TopologyObserver: Send + Sync {
    /// A child terminal was projected and its tree save won the revision CAS.
    fn turn_usage(&self, terminal: super::ProjectedChildTurnUsage);
    /// A child node was admitted and its rows written.
    fn node_spawned(
        &self,
        tree_id: &str,
        node_id: &str,
        parent_node_id: Option<&str>,
        template: &str,
    );
    /// A bound non-root node's session ended. Emitted once per node lifetime.
    fn node_detached(
        &self,
        tree_id: &str,
        node_id: &str,
        parent_node_id: Option<&str>,
        status: NodeDetach,
    );
    /// A successor epoch was installed on the tree.
    fn epoch_advanced(&self, tree_id: &str);
}
