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
    /// A capacity refusal retains its typed recovery-storage classification.
    fn save_tree(
        &self,
        tree: &mut ProductAgentTree,
    ) -> Result<(), crate::agent_tree::TopologyError>;

    /// Delete one tree's rows, compare-and-swapped on `expected_revision`;
    /// an absent tree is not an error.
    ///
    /// # Errors
    /// A moved revision, or rows that cannot delete trees.
    fn delete_tree(&self, tree_id: &str, expected_revision: u64) -> Result<(), String> {
        let _ = (tree_id, expected_revision);
        Err("these tree rows do not delete trees".to_owned())
    }
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
    let mut next_source = inherited_tree_source(session);
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
            .and_then(|record| inherited_tree_source(&record));
        candidate = source;
    }
    session.id.clone()
}

/// The source whose tree `session` may still be on. A record whose node is
/// the root of a tree named after itself opened a tree of its own (a new
/// session, `/new`, a fork, a carry or import, a rebind onto its own tree)
/// and inherits none; only a successor that stayed on its source's tree
/// does. A carried-forward source lives in another build's store.
fn inherited_tree_source(session: &SessionRecord) -> Option<String> {
    if session.node.tree_id == session.id || session.carried_forward_from().is_some() {
        return None;
    }
    session.source_product_session_id.clone()
}

/// Whether the tree must be resumed explicitly before it admits work.
///
/// Attached trees expose their last installed admission state without taking
/// the tree mutex. A busy tree is not a paused tree.
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
/// The same lock-free state used by admission: observation cadence and a busy
/// tree mutex cannot hide a real pause or invent one.
pub(crate) fn observed_restored_paused(rows: &dyn TreeRows, tree_id: &str) -> bool {
    if let Some(paused) = crate::topology_runtime::hub_tree_is_restored_paused(tree_id) {
        return paused;
    }
    matches!(
        rows.tree(tree_id),
        Ok(Some(tree)) if tree.state == TreeAdmissionState::RestoredPaused
    )
}

/// Operator-facing reason when rebind is blocked.
///
/// Store-open or load failure fails closed as live descendants, not quiescent.
pub fn epoch_rebind_refusal(
    rows: &dyn TreeRows,
    tree_id: &str,
) -> Option<crate::agent_tree::EpochRebindRefusal> {
    if let Some(runtime) = lookup(tree_id) {
        return runtime
            .read(ProductAgentTree::epoch_rebind_refusal_kind)
            .ok()
            .flatten();
    }
    match rows.tree(tree_id) {
        Ok(Some(tree)) => tree.epoch_rebind_refusal_kind(),
        Ok(None) => None,
        Err(_) => Some(crate::agent_tree::EpochRebindRefusal::LiveDescendants),
    }
}

/// Explicit ResumeTree on a restored tree; a tree that is not paused is a
/// no-op, and a tree with no hub cannot be resumed at all.
pub async fn resume_tree(tree_id: &str) -> Result<(), String> {
    let Some(runtime) = lookup(tree_id) else {
        return Err("topology tree is missing from the hub; cannot ResumeTree".to_owned());
    };
    runtime
        .mutate_then_emit(false, |tree| {
            if tree.state != TreeAdmissionState::RestoredPaused {
                return Ok(());
            }
            tree.resume_tree().map_err(|error| error.message)
        })
        .await
}

/// Root Ctrl-C: cancels this turn and interrupts every running descendant,
/// without closing any. Returns how many children were interrupted.
///
/// Records the interrupt and signals children; release owners settle in the
/// background and retain their own failure evidence.
pub(crate) async fn mark_root_interrupted(tree_id: &str) -> Result<usize, String> {
    interrupt_root(tree_id, false).await
}

/// A headless run is leaving its executor: join within the shutdown allowance
/// so healthy child terminal/job publications finish before process teardown.
pub(crate) async fn mark_root_interrupted_for_run(tree_id: &str) -> Result<usize, String> {
    interrupt_root(tree_id, true).await
}

async fn interrupt_root(tree_id: &str, exiting: bool) -> Result<usize, String> {
    let Some(runtime) = lookup(tree_id) else {
        return Ok(0);
    };
    let mut interrupted = runtime
        .mutate_then_emit(false, |tree| {
            let interrupted = tree
                .interrupt_running_descendants()
                .map_err(|error| error.message)?;
            tree.mark_root_interrupted();
            Ok(interrupted)
        })
        .await?;
    let count = interrupted.len();
    if exiting {
        interrupted = runtime.live.lock().await.keys().cloned().collect();
    }
    cancel_interrupted_children(&runtime, &interrupted, exiting).await?;
    Ok(count)
}

async fn cancel_interrupted_children(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    interrupted: &[String],
    exiting: bool,
) -> Result<(), String> {
    if interrupted.is_empty() && !exiting {
        return Ok(());
    }
    if exiting {
        let report = tokio::time::timeout(
            crate::topology_runtime::HOST_SHUTDOWN_RELEASE_TIMEOUT,
            async {
                let join = runtime.begin_cancel_live_many(interrupted).await;
                let report = join.await;
                runtime.driver().join_retirement_tasks().await?;
                Ok::<_, String>(report)
            },
        )
        .await
        .map_err(|_| "child retirement exceeded the run-exit bound; owner retained".to_owned())??;
        return report.first_failure.map_or(Ok(()), Err);
    }
    let join = runtime.begin_cancel_live_many(interrupted).await;
    runtime.driver().tasks().spawn(async move {
        if let Some(error) = join.await.first_failure {
            tracing::error!(target: "plak.topology", error, "interrupted child retirement failed");
        }
    });
    Ok(())
}

/// Next root prompt on the same frozen tuple returns Live.
pub(crate) async fn mark_root_live_for_prompt(tree_id: &str) -> Result<(), String> {
    let Some(runtime) = lookup(tree_id) else {
        return Ok(());
    };
    runtime
        .mutate_then_emit(false, |tree| {
            tree.mark_root_live_for_prompt();
            Ok(())
        })
        .await
}

/// Freeze the next epoch for a tree whose root just rebound.
pub async fn install_successor_epoch(
    rows: &dyn TreeRows,
    snapshot: OrchestrationSnapshot,
    tree_id: &str,
    emit: &dyn Fn(crate::product_observation::ProductObservation),
) -> Result<(), String> {
    if let Some(runtime) = lookup(tree_id) {
        runtime
            .mutate_then_emit(true, |tree| {
                tree.begin_successor_epoch(snapshot)
                    .map_err(|error| error.message)
            })
            .await?;
        return Ok(());
    }
    install_successor_epoch_headless(rows, snapshot, tree_id, emit)
}

/// A model rebind that stays on its tree keeps that tree's roster: the
/// successor epoch is the current one with only the root's account, model
/// and effort moved. A tree with no rows installs nothing.
pub async fn install_root_bind_epoch(
    rows: &dyn TreeRows,
    tree_id: &str,
    account: &str,
    tuple_id: &str,
    effort: Option<&str>,
    emit: &dyn Fn(crate::product_observation::ProductObservation),
) -> Result<(), String> {
    let (account, tuple_id, effort) = (
        account.to_owned(),
        tuple_id.to_owned(),
        effort.map(str::to_owned),
    );
    let successor = move |tree: &ProductAgentTree| {
        tree.current_epoch()
            .with_root_bind(account, tuple_id, effort)
    };
    if let Some(runtime) = lookup(tree_id) {
        // The successor is derived inside the writer, from the epoch current
        // at its turn.
        runtime
            .mutate_then_emit(true, move |tree| {
                let snapshot = successor(tree);
                tree.begin_successor_epoch(snapshot)
                    .map_err(|error| error.message)
            })
            .await?;
        return Ok(());
    }
    let Some(tree) = rows.tree(tree_id)? else {
        return Ok(());
    };
    install_on_loaded(rows, tree, successor, tree_id, emit)
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
    let Some(tree) = rows.tree(tree_id)? else {
        return Ok(());
    };
    install_on_loaded(rows, tree, |_| snapshot, tree_id, emit)
}

fn install_on_loaded(
    rows: &dyn TreeRows,
    mut tree: ProductAgentTree,
    successor: impl FnOnce(&ProductAgentTree) -> OrchestrationSnapshot,
    tree_id: &str,
    emit: &dyn Fn(crate::product_observation::ProductObservation),
) -> Result<(), String> {
    let snapshot = successor(&tree);
    tree.begin_successor_epoch(snapshot)
        .map_err(|error| error.message)?;
    rows.save_tree(&mut tree).map_err(|error| error.message)?;
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

    fn save_tree(
        &self,
        tree: &mut ProductAgentTree,
    ) -> Result<(), crate::agent_tree::TopologyError> {
        let acknowledged = self
            .trees()
            .map_err(|message| {
                crate::agent_tree::TopologyError::new(
                    crate::agent_tree_protocol::TopologyErrorCode::OperationRecoveryRequired,
                    message,
                )
            })?
            .save(tree)
            .map_err(crate::agent_tree::TopologyError::from)?;
        tree.acknowledge_revision(acknowledged);
        Ok(())
    }

    fn delete_tree(&self, tree_id: &str, expected_revision: u64) -> Result<(), String> {
        self.trees()?
            .delete(tree_id, expected_revision)
            .map_err(|error| error.detail.clone())
    }
}

impl TopologyRows for HostRows<'_> {
    fn session(&self, session_id: &str) -> Result<SessionRecord, String> {
        self.records.load(session_id)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
