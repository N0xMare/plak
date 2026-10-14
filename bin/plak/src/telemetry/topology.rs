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
mod tests {

    use plakous_recursive_subagents::protocol::{
        AttemptScopedRequest, NodeStatus, SpawnAgentRequest, WriterMode,
    };

    use plakous_recursive_subagents::tree::{NodeDetach, ProductAgentTree};

    use tempfile::tempdir;

    fn spawn_req() -> SpawnAgentRequest {
        SpawnAgentRequest::test_review()
    }

    fn close_req(agent_id: &str) -> AttemptScopedRequest {
        AttemptScopedRequest {
            agent_id: agent_id.to_owned(),
            expected_attempt_id: None,
        }
    }

    #[test]
    fn shutdown_detach_emits_only_for_non_finished_bound_nodes() {
        use crate::telemetry_sink::TelemetryWriter;
        use crate::telemetry_store::TelemetryItem;
        use plakous_hosted::product_observation::{NodeDetachStatus, ProductObservation};
        use std::sync::mpsc::TryRecvError;

        let tree_id = uuid::Uuid::new_v4().to_string();
        let root = tempdir().unwrap();
        let mut tree = ProductAgentTree::bootstrap_root_for_test(
            tree_id,
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let closed = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.bind_session_locator(&closed.agent_id, "sess/closed")
            .unwrap();
        let live = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.bind_session_locator(&live.agent_id, "sess/live")
            .unwrap();
        tree.close_agent("root", &close_req(&closed.agent_id), None)
            .unwrap();
        tree.settle_for_test();
        assert_eq!(
            tree.node(&closed.agent_id).unwrap().status,
            NodeStatus::Closed
        );

        let (writer, receiver) = TelemetryWriter::detached(4);
        let observer = crate::telemetry::topology::ProductTopologyObserver::port(Some(writer))
            .expect("a writer yields the product observer");
        tree.emit_shutdown_detaches(Some(&observer));

        let mut detached = 0_u32;
        let mut node_id = None;
        let mut status = None;
        loop {
            match receiver.try_recv() {
                Ok(TelemetryItem::Product(ProductObservation::TreeNodeDetached {
                    node_id: id,
                    status: detached_as,
                    ..
                })) => {
                    detached += 1;
                    node_id = Some(id.as_str().to_owned());
                    status = Some(detached_as);
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        assert_eq!(detached, 1);
        assert_eq!(node_id.as_deref(), Some(live.agent_id.as_str()));
        assert_eq!(status, Some(NodeDetachStatus::Shutdown));
    }

    #[test]
    fn detach_guard_does_not_consume_on_pre_emit_validation_failure() {
        use crate::telemetry_sink::TelemetryWriter;
        use crate::telemetry_store::TelemetryItem;
        use plakous_hosted::product_observation::ProductObservation;
        use std::sync::mpsc::TryRecvError;

        let tree_id = uuid::Uuid::new_v4().to_string();
        let root = tempdir().unwrap();
        let mut tree = ProductAgentTree::bootstrap_root_for_test(
            tree_id,
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let (writer, receiver) = TelemetryWriter::detached(4);
        let observer = crate::telemetry::topology::ProductTopologyObserver::port(Some(writer))
            .expect("a writer yields the product observer");

        tree.try_emit_tree_node_detached(Some(&observer), &spawned.agent_id, NodeDetach::Failed);
        tree.bind_session_locator(&spawned.agent_id, "sess/live")
            .unwrap();
        tree.try_emit_tree_node_detached(Some(&observer), &spawned.agent_id, NodeDetach::Closed);
        let mut detached = 0_u32;
        loop {
            match receiver.try_recv() {
                Ok(TelemetryItem::Product(ProductObservation::TreeNodeDetached { .. })) => {
                    detached += 1;
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        assert_eq!(detached, 1);
    }

    #[test]
    fn recovery_detach_emits_without_bound_locator() {
        use crate::telemetry_sink::TelemetryWriter;
        use crate::telemetry_store::TelemetryItem;
        use plakous_hosted::product_observation::{NodeDetachStatus, ProductObservation};
        use std::sync::mpsc::TryRecvError;

        let tree_id = uuid::Uuid::new_v4().to_string();
        let root = tempdir().unwrap();
        let mut tree = ProductAgentTree::bootstrap_root_for_test(
            tree_id,
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_host_materialization(&spawned.agent_id)
            .and_then(|value| tree.fold_due(&spawned.agent_id).map(|_| value))
            .unwrap();
        tree.settle_for_test();
        let (writer, receiver) = TelemetryWriter::detached(4);
        let observer = crate::telemetry::topology::ProductTopologyObserver::port(Some(writer))
            .expect("a writer yields the product observer");
        tree.try_emit_tree_node_detached(
            Some(&observer),
            &spawned.agent_id,
            NodeDetach::RecoveryRequired,
        );
        let mut detached = 0_u32;
        loop {
            match receiver.try_recv() {
                Ok(TelemetryItem::Product(ProductObservation::TreeNodeDetached {
                    node_id,
                    status,
                    ..
                })) => {
                    detached += 1;
                    assert_eq!(node_id.as_str(), spawned.agent_id);
                    assert_eq!(status, NodeDetachStatus::RecoveryRequired);
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        assert_eq!(detached, 1);
    }
}
