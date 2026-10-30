//! The product's implementation of the tree's telemetry port.
//!
//! `plakous-recursive-subagents` reports node lifetimes and epoch installs as
//! ids and a status; this is where those become the product's
//! `ProductObservation` vocabulary and reach the telemetry store. The mapping
//! is the only place the two vocabularies meet, and it drops anything that
//! will not fit the bounded wire rather than truncating it.

use std::sync::Arc;

use crate::telemetry_sink::TelemetryWriter;
use plakous_hosted::agent_tree::{NodeDetach, TopologyObserver};
use plakous_hosted::product_observation::{
    BoundedText64, NodeDetachStatus, ProductObservation, session_id_from_text, wall_timestamp,
};

/// Bridges the tree's observer port onto one runtime's telemetry writer.
pub struct ProductTopologyObserver {
    writer: Arc<TelemetryWriter>,
}

impl ProductTopologyObserver {
    #[must_use]
    pub const fn new(writer: Arc<TelemetryWriter>) -> Self {
        Self { writer }
    }

    /// The port's handle shape: `None` when the runtime carries no writer, so
    /// a tree without telemetry holds no observer at all.
    #[must_use]
    pub fn port(writer: Option<Arc<TelemetryWriter>>) -> Option<Arc<dyn TopologyObserver>> {
        writer.map(|writer| Arc::new(Self::new(writer)) as Arc<dyn TopologyObserver>)
    }
}

const fn detach_status(status: NodeDetach) -> NodeDetachStatus {
    match status {
        NodeDetach::Completed => NodeDetachStatus::Completed,
        NodeDetach::Failed => NodeDetachStatus::Failed,
        NodeDetach::Closed => NodeDetachStatus::Closed,
        NodeDetach::RecoveryRequired => NodeDetachStatus::RecoveryRequired,
        NodeDetach::Shutdown => NodeDetachStatus::Shutdown,
    }
}

impl TopologyObserver for ProductTopologyObserver {
    fn turn_usage(&self, terminal: plakous_hosted::agent_tree::ProjectedChildTurnUsage) {
        let Ok(at) = plakous::model::Timestamp::new(
            terminal.committed_at_ms / 1_000,
            u32::try_from(terminal.committed_at_ms % 1_000).unwrap_or(0) * 1_000_000,
        ) else {
            return;
        };
        self.writer.emit_product(ProductObservation::TurnUsage {
            at,
            session_id: terminal.session_id,
            branch_id: terminal.branch_id,
            turn_id: terminal.turn_id,
            usage: terminal.usage,
        });
    }
    fn node_spawned(
        &self,
        tree_id: &str,
        node_id: &str,
        parent_node_id: Option<&str>,
        template: &str,
    ) {
        let (Some(session_id), Some(tree_id), Some(node_id)) = (
            session_id_from_text(tree_id),
            BoundedText64::new(tree_id),
            BoundedText64::new(node_id),
        ) else {
            return;
        };
        self.writer
            .emit_product(ProductObservation::TreeNodeSpawned {
                at: wall_timestamp(),
                session_id,
                tree_id,
                node_id,
                parent_node_id: parent_node_id.and_then(BoundedText64::new),
                // A template name too long for the bounded wire is reported as
                // `other` rather than dropping the whole spawn.
                template: BoundedText64::new(template).unwrap_or_else(|| {
                    BoundedText64::new("other").unwrap_or_else(|| unreachable!("bounded label"))
                }),
            });
    }

    fn node_detached(
        &self,
        tree_id: &str,
        node_id: &str,
        parent_node_id: Option<&str>,
        status: NodeDetach,
    ) {
        let (Some(session_id), Some(tree_id), Some(node_id)) = (
            session_id_from_text(tree_id),
            BoundedText64::new(tree_id),
            BoundedText64::new(node_id),
        ) else {
            return;
        };
        self.writer
            .emit_product(ProductObservation::TreeNodeDetached {
                at: wall_timestamp(),
                session_id,
                tree_id,
                node_id,
                parent_node_id: parent_node_id.and_then(BoundedText64::new),
                status: detach_status(status),
            });
    }

    fn epoch_advanced(&self, tree_id: &str) {
        let (Some(session_id), Some(tree_id)) =
            (session_id_from_text(tree_id), BoundedText64::new(tree_id))
        else {
            return;
        };
        self.writer
            .emit_product(ProductObservation::TreeEpochAdvanced {
                at: wall_timestamp(),
                session_id,
                tree_id,
            });
    }
}

#[cfg(test)]
#[path = "topology/usage_tests.rs"]
mod usage_tests;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
