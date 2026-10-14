//! The durable topology questions a hosted session asks about its own tree.
//!
//! "Which tree is this session's?", "is it paused?", "may it rebind?", "mark
//! the root interrupted" — every one of them is answered from the hub when the
//! tree is live and from the durable rows when it is not, and none of them is
//! product policy. They sat in the product's topology module behind a
//! `PlakPaths` because that is where the rows were opened; the rows are a port
//! now, so the questions come here and the product keeps only its own way of
//! opening them.

use std::sync::Arc;

use crate::agent_tree::{ProductAgentTree, TreeAdmissionState};
use crate::orchestration::OrchestrationSnapshot;
use crate::sessions::SessionRecord;
use crate::topology_runtime::{is_restored_paused, lookup};

/// The durable rows behind the questions in this module.
///
/// Two implementations: the hosted session's `ProductStateRoots`, which reads
/// through the ports the product supplied, and the product's own tree paths
/// for a headless CLI flow. Session ancestry additionally needs `TopologyRows`.
pub trait TreeRows {
    /// The persisted tree with the reopen law applied.
    fn tree(&self, tree_id: &str) -> Result<Option<ProductAgentTree>, String>;

    /// Whether a tree is persisted at all.
    fn tree_exists(&self, tree_id: &str) -> Result<bool, String>;

    /// Publish a whole tree through the store's compare-and-swap.
    fn save_tree(&self, tree: &mut ProductAgentTree) -> Result<(), String>;
}

/// Tree rows plus session ancestry through the supplied product record service.
pub trait TopologyRows: TreeRows {
    /// One persisted session record.
    fn session(&self, session_id: &str) -> Result<SessionRecord, String>;
}

/// Stable tree identity across rebind branches. Rebind walks
/// `source_product_session_id`
/// until a hub or stored tree exists, then that genesis id.
pub fn durable_tree_id(rows: &dyn TopologyRows, session: &SessionRecord) -> String {
    let mut candidate = session.id.clone();
    // A carried-forward source lives in another build's store.
    let mut next_source = session
        .carried_forward_from()
        .is_none()
        .then(|| session.source_product_session_id.clone())
        .flatten();
    let mut seen = std::collections::BTreeSet::new();
    loop {
        if !seen.insert(candidate.clone()) {
            break;
        }
        if lookup(&candidate).is_some() {
            return candidate;
        }
        if rows.tree_exists(&candidate).unwrap_or(false) {
            return candidate;
        }
        let Some(source) = next_source else {
            break;
        };
        next_source = rows
            .session(&source)
            .ok()
            .filter(|record| record.carried_forward_from().is_none())
            .and_then(|record| record.source_product_session_id);
        candidate = source;
    }
    session.id.clone()
}

/// Whether the tree must be resumed explicitly before it admits work.
///
/// Fails closed: a contended hub lock reads as paused, because a gate that
/// admits work must not admit on "busy".
pub fn requires_explicit_resume(rows: &dyn TreeRows, tree_id: &str) -> bool {
    if is_restored_paused(tree_id) {
        return true;
    }
    matches!(
        rows.tree(tree_id),
        Ok(Some(tree)) if tree.state == TreeAdmissionState::RestoredPaused
    )
}

/// Whether the host has *observed* this tree paused, for operator copy.
///
/// [`requires_explicit_resume`] deliberately reads a contended hub lock as
/// paused. A note or the HUD must not, because it would tell the operator
/// their tree is paused whenever the hub is merely busy — which is exactly what
/// a fresh attach that spawns a subagent looks like. Contention here means "not
/// observed", so nothing is claimed.
pub(crate) fn observed_restored_paused(rows: &dyn TreeRows, tree_id: &str) -> bool {
    if let Some(runtime) = lookup(tree_id) {
        return runtime
            .try_read(|tree| tree.state == TreeAdmissionState::RestoredPaused)
            .unwrap_or(false);
    }
    matches!(
        rows.tree(tree_id),
        Ok(Some(tree)) if tree.state == TreeAdmissionState::RestoredPaused
    )
}

/// Operator-facing reason when rebind is blocked.
///
/// Store-open or load failure fails closed as live descendants, not quiescent.
pub fn epoch_rebind_refusal(rows: &dyn TreeRows, tree_id: &str) -> Option<&'static str> {
    if let Some(runtime) = lookup(tree_id) {
        return runtime
            .read(ProductAgentTree::epoch_rebind_refusal)
            .ok()
            .flatten();
    }
    match rows.tree(tree_id) {
        Ok(Some(tree)) => tree.epoch_rebind_refusal(),
        Ok(None) => None,
        Err(_) => Some(ProductAgentTree::TREE_EPOCH_REBIND_LIVE_DESCENDANTS),
    }
}

/// Explicit ResumeTree on a restored tree; a tree that is not paused is a
/// no-op, and a tree with no hub cannot be resumed at all.
pub fn resume_tree(tree_id: &str) -> Result<(), String> {
    let Some(runtime) = lookup(tree_id) else {
        return Err("topology tree is missing from the hub; cannot ResumeTree".to_owned());
    };
    runtime.mutate_then_emit(false, |tree| {
        if tree.state != TreeAdmissionState::RestoredPaused {
            return Ok(());
        }
        tree.resume_tree().map_err(|error| error.message)
    })
}

/// Root Ctrl-C: cancels this turn and interrupts every running descendant,
/// without closing any. Returns how many children were interrupted.
///
/// Joins the children's bounded retirement before the caller may quit. Failed
/// cancellation is recorded as that child's retirement failure.
pub(crate) async fn mark_root_interrupted(tree_id: &str) -> Result<usize, String> {
    let Some(runtime) = lookup(tree_id) else {
        return Ok(0);
    };
    let mut interrupted = Vec::new();
    let publication = runtime.mutate_then_emit(false, |tree| {
        interrupted = tree
            .interrupt_running_descendants()
            .map_err(|error| error.message)?;
        tree.mark_root_interrupted();
        Ok(())
    });
    let count = interrupted.len();
    let cancellation = cancel_interrupted_children(&runtime, &interrupted).await;
    publication?;
    cancellation?;
    Ok(count)
}

async fn cancel_interrupted_children(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    interrupted: &[String],
) -> Result<(), String> {
    if interrupted.is_empty() {
        return Ok(());
    }
    let report = runtime.cancel_live_many(interrupted).await;
    if report.is_clean() {
        return Ok(());
    }
    let message = report
        .first_failure
        .unwrap_or_else(|| "child turn could not be cancelled".to_owned());
    for owner in &report.owner_failures {
        let _ = runtime.record_then_apply(&owner.agent_id, |tree| {
            tree.record_host_retirement_for_attempt(
                &owner.agent_id,
                owner.expected_attempt_id.as_ref(),
                owner.message.clone(),
            )
        });
    }
    if !report.owner_failures.is_empty() {
        runtime.driver().request_retirement_recovery();
    }
    Err(message)
}

/// Next root prompt on the same frozen tuple returns Live.
pub(crate) fn mark_root_live_for_prompt(tree_id: &str) -> Result<(), String> {
    let Some(runtime) = lookup(tree_id) else {
        return Ok(());
    };
    runtime.mutate_then_emit(false, |tree| {
        tree.mark_root_live_for_prompt();
        Ok(())
    })
}

/// Freeze the next epoch for a tree whose root just rebound.
pub fn install_successor_epoch(
    rows: &dyn TreeRows,
    snapshot: OrchestrationSnapshot,
    tree_id: &str,
    emit: &dyn Fn(crate::product_observation::ProductObservation),
) -> Result<(), String> {
    if let Some(runtime) = lookup(tree_id) {
        runtime.mutate_then_emit(true, |tree| {
            tree.begin_successor_epoch(snapshot)
                .map_err(|error| error.message)
        })?;
        return Ok(());
    }
    install_successor_epoch_headless(rows, snapshot, tree_id, emit)
}

/// The no-hub half of [`install_successor_epoch`]: a headless `plak config
/// set` in another process than the TUI. Its save is the store's
/// compare-and-swap, so when a hub in that other process publishes first this
/// refuses instead of overwriting, and when this publishes first the hub's
/// next save is refused — either way the operator hears about it.
pub fn install_successor_epoch_headless(
    rows: &dyn TreeRows,
    snapshot: OrchestrationSnapshot,
    tree_id: &str,
    emit: &dyn Fn(crate::product_observation::ProductObservation),
) -> Result<(), String> {
    let Some(mut tree) = rows.tree(tree_id)? else {
        return Ok(());
    };
    tree.begin_successor_epoch(snapshot)
        .map_err(|error| error.message)?;
    rows.save_tree(&mut tree)?;
    if let (Some(session_id), Some(tree_id_text)) = (
        crate::product_observation::session_id_from_text(tree_id),
        crate::product_observation::BoundedText64::new(tree_id),
    ) {
        emit(
            crate::product_observation::ProductObservation::TreeEpochAdvanced {
                at: crate::product_observation::wall_timestamp(),
                session_id,
                tree_id: tree_id_text,
            },
        );
    }
    Ok(())
}

/// The hosted session's rows: the tree store the product supplied to the state
/// roots, and the session-record port on its `HostConfig`.
pub struct HostRows<'a> {
    roots: &'a crate::agent::ProductStateRoots,
    records: &'a dyn crate::session_store::SessionRecordStore,
}

impl<'a> HostRows<'a> {
    #[must_use]
    pub const fn new(
        roots: &'a crate::agent::ProductStateRoots,
        records: &'a dyn crate::session_store::SessionRecordStore,
    ) -> Self {
        Self { roots, records }
    }

    fn trees(&self) -> Result<Arc<dyn crate::agent_tree::TreeStore>, String> {
        self.roots
            .tree_store()
            .map_err(|failure| failure.message().to_owned())
    }
}

impl TreeRows for HostRows<'_> {
    fn tree(&self, tree_id: &str) -> Result<Option<ProductAgentTree>, String> {
        Ok(self
            .trees()?
            .load(tree_id)
            .map_err(|error| error.detail.clone())?
            .map(ProductAgentTree::reopen))
    }

    fn tree_exists(&self, tree_id: &str) -> Result<bool, String> {
        self.trees()?
            .exists(tree_id)
            .map_err(|error| error.detail.clone())
    }

    fn save_tree(&self, tree: &mut ProductAgentTree) -> Result<(), String> {
        let acknowledged = self
            .trees()?
            .save(tree)
            .map_err(|error| error.detail.clone())?;
        tree.acknowledge_revision(acknowledged);
        Ok(())
    }
}

impl TopologyRows for HostRows<'_> {
    fn session(&self, session_id: &str) -> Result<SessionRecord, String> {
        self.records.load(session_id)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use crate::agent_tree_protocol::{NodeStatus, SpawnAgentRequest};

    struct DelayedChild(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl crate::topology_runtime::LiveChildHandle for DelayedChild {
        fn agent(&self) -> Option<plakous::agent::Agent> {
            None
        }
        fn turn_id(&self) -> plakous::model::TurnId {
            "00000000-0000-0000-0000-000000000001".parse().unwrap()
        }
        fn turn_control(&self) -> plakous::agent::TurnControl {
            panic!("unused")
        }
        fn follow_up(
            &self,
            _: plakous::agent::CommandId,
            _: String,
        ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }
        fn restart(
            &mut self,
            _: crate::agent_tree_tools::TreeDriver,
            _: plakous::agent::CommandId,
            _: String,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<plakous::agent::TurnResultHandle, String>> + Send + '_>,
        > {
            Box::pin(async { Err("unused".into()) })
        }
        fn release(
            self: Box<Self>,
            driver: crate::agent_tree_tools::TreeDriver,
        ) -> std::pin::Pin<Box<dyn Future<Output = crate::topology_runtime::LiveRelease> + Send>>
        {
            self.cancel_and_release(driver)
        }
        fn cancel_and_release(
            self: Box<Self>,
            _driver: crate::agent_tree_tools::TreeDriver,
        ) -> std::pin::Pin<Box<dyn Future<Output = crate::topology_runtime::LiveRelease> + Send>>
        {
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
                crate::topology_runtime::LiveRelease::default()
            })
        }
    }

    #[test]
    fn root_interrupt_from_async_drive_joins_live_children_before_returning() {
        let root = tempfile::tempdir().unwrap();
        let tree_id = uuid::Uuid::new_v4().to_string();
        let runtime = crate::runtime::test_kit::install_idle_topology_hub(&tree_id, root.path());
        let child = runtime
            .mutate_then_emit(false, |tree| {
                let id = tree
                    .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                    .unwrap()
                    .agent_id;
                tree.set_active_turn(&id, true).unwrap();
                Ok(id)
            })
            .unwrap();
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let executor = plakous::application::ApplicationExecutor::new().unwrap();
        assert!(
            executor
                .block_on(
                    runtime
                        .try_occupy_live(child.clone(), Box::new(DelayedChild(cancelled.clone())))
                )
                .unwrap()
                .is_ok()
        );
        assert_eq!(
            executor
                .block_on(super::mark_root_interrupted(&tree_id))
                .unwrap()
                .unwrap(),
            1
        );
        assert!(
            cancelled.load(std::sync::atomic::Ordering::SeqCst),
            "run exit must wait for child retirement"
        );
        executor
            .block_on(crate::topology_runtime::force_release(&tree_id))
            .unwrap();
    }

    /// A root cancel interrupts the children that are running, so they stop
    /// spending, and reports how many; a tree with none says zero.
    #[test]
    fn a_root_interrupt_stops_running_children() {
        let root = tempfile::tempdir().unwrap();
        let tree_id = uuid::Uuid::new_v4().to_string();
        let runtime = crate::runtime::test_kit::install_idle_topology_hub(&tree_id, root.path());
        let executor = plakous::application::ApplicationExecutor::new().unwrap();
        assert_eq!(
            executor
                .block_on(super::mark_root_interrupted(&tree_id))
                .unwrap()
                .unwrap(),
            0
        );
        let children = runtime
            .mutate_then_emit(false, |tree| {
                let mut ids = Vec::new();
                for _ in 0..2 {
                    let id = tree
                        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                        .unwrap()
                        .agent_id;
                    tree.set_active_turn(&id, true).unwrap();
                    ids.push(id);
                }
                Ok(ids)
            })
            .unwrap();
        assert_eq!(
            executor
                .block_on(super::mark_root_interrupted(&tree_id))
                .unwrap()
                .unwrap(),
            2
        );
        for id in &children {
            assert_eq!(
                runtime.read(|tree| tree.node(id).unwrap().status).unwrap(),
                NodeStatus::Interrupted
            );
        }
        plakous::application::ApplicationExecutor::new()
            .unwrap()
            .block_on(crate::topology_runtime::force_release(&tree_id))
            .unwrap();
    }
}
