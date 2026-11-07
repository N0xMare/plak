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

use crate::agent_tree::ProductAgentTree;
use crate::orchestration::OrchestrationSnapshot;
use crate::sessions::SessionRecord;
use crate::topology_runtime::lookup;

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
    /// A capacity refusal retains its typed recovery-storage classification,
    /// and another host's custody its typed refusal.
    fn save_tree(&self, tree: &mut ProductAgentTree) -> Result<(), TreeWriteError>;

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

/// A refused tree write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TreeWriteError {
    /// Another live host process holds the tree's custody; nothing was
    /// written.
    HostedElsewhere(Box<crate::agent_tree::TreeHostedElsewhere>),
    /// Any other refusal, under its topology classification.
    Refused(crate::agent_tree::TopologyError),
}

impl TreeWriteError {
    /// The operator-facing detail.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::HostedElsewhere(refusal) => refusal.to_string(),
            Self::Refused(error) => error.message.clone(),
        }
    }
}

impl std::fmt::Display for TreeWriteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message())
    }
}

impl std::error::Error for TreeWriteError {}

impl From<crate::agent_tree::TreeStoreError> for TreeWriteError {
    fn from(error: crate::agent_tree::TreeStoreError) -> Self {
        match error.kind {
            crate::agent_tree::TreeStoreErrorKind::HostedElsewhere(refusal) => {
                Self::HostedElsewhere(refusal)
            }
            _ => Self::Refused(error.into()),
        }
    }
}

impl From<crate::agent_tree::TopologyError> for TreeWriteError {
    fn from(error: crate::agent_tree::TopologyError) -> Self {
        Self::Refused(error)
    }
}

/// A topology tool reads the custody refusal as the recovery-required
/// refusal the store conversion has always given it.
impl From<TreeWriteError> for crate::agent_tree::TopologyError {
    fn from(error: TreeWriteError) -> Self {
        match error {
            TreeWriteError::HostedElsewhere(refusal) => {
                crate::agent_tree::TreeStoreError::hosted_elsewhere(*refusal).into()
            }
            TreeWriteError::Refused(error) => error,
        }
    }
}

impl From<TreeWriteError> for crate::error::ReadFailure {
    fn from(error: TreeWriteError) -> Self {
        match error {
            TreeWriteError::HostedElsewhere(refusal) => Self::refused(
                crate::error::StoreRefusal::HostedElsewhere,
                refusal.to_string(),
            ),
            TreeWriteError::Refused(error) => error.message.into(),
        }
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

/// How a tree stop treats the children it interrupted.
enum ChildStop {
    /// The root's Ctrl-C: cascade, and settle the release in the background.
    Cascade,
    /// A running turn's stop: record `stopped` and signal, without joining.
    /// The idle sweep joins.
    Signal,
    /// The idle sweep, or a run leaving its executor: record `stopped` and
    /// join within the shutdown allowance.
    Join,
}

/// Root Ctrl-C: cancels this turn and interrupts every running descendant,
/// without closing any. Returns how many children were interrupted.
///
/// Records the interrupt and signals children; release owners settle in the
/// background and retain their own failure evidence.
pub(crate) async fn mark_root_interrupted(tree_id: &str) -> Result<usize, String> {
    interrupt_root(tree_id, ChildStop::Cascade).await
}

/// A running turn's stop: record `stopped` and signal the children, and do
/// not join them. The root's cancel stays immediate; the idle sweep joins.
pub(crate) async fn signal_root_stopped(tree_id: &str) -> Result<usize, String> {
    interrupt_root(tree_id, ChildStop::Signal).await
}

/// The idle sweep, or a headless run leaving its executor: join within the
/// shutdown allowance so child terminal and job publications finish.
pub(crate) async fn mark_root_interrupted_for_run(tree_id: &str) -> Result<usize, String> {
    interrupt_root(tree_id, ChildStop::Join).await
}

async fn interrupt_root(tree_id: &str, stop: ChildStop) -> Result<usize, String> {
    let Some(runtime) = lookup(tree_id) else {
        return Ok(0);
    };
    if matches!(stop, ChildStop::Join) {
        join_stop_recoveries(&runtime).await?;
    }
    let cause = match stop {
        ChildStop::Cascade => crate::agent_tree::InterruptCause::Cascade,
        ChildStop::Signal | ChildStop::Join => crate::agent_tree::InterruptCause::Stopped,
    };
    let mut interrupted = runtime
        .mutate_then_emit(false, move |tree| {
            let interrupted = tree
                .interrupt_running_descendants_for(cause)
                .map_err(|error| error.message)?;
            tree.mark_root_interrupted();
            Ok(interrupted)
        })
        .await?;
    let count = interrupted.len();
    let join = matches!(stop, ChildStop::Join);
    if join {
        interrupted = runtime.live.lock().await.keys().cloned().collect();
    }
    cancel_interrupted_children(&runtime, &interrupted, join, tree_id).await?;
    Ok(count)
}

async fn join_stop_recoveries(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
) -> Result<(), String> {
    let deadline =
        tokio::time::Instant::now() + crate::topology_runtime::HOST_SHUTDOWN_RELEASE_TIMEOUT;
    loop {
        let mut tasks = runtime.take_recoveries();
        if tasks.is_empty() {
            return Ok(());
        }
        let joined = tokio::time::timeout_at(deadline, async {
            for task in &mut tasks {
                task.await
                    .map_err(|_| "child recovery failed before the session stop".to_owned())?;
            }
            Ok(())
        })
        .await
        .unwrap_or_else(|_| Err("child recovery exceeded the session stop bound".to_owned()));
        if let Err(error) = joined {
            for task in tasks {
                runtime.track_recovery(task);
            }
            return Err(error);
        }
    }
}

async fn cancel_interrupted_children(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    interrupted: &[String],
    exiting: bool,
    #[cfg_attr(
        not(test),
        allow(
            unused_variables,
            reason = "the tree id is read only by the test retirement seam"
        )
    )]
    tree_id: &str,
) -> Result<(), String> {
    if interrupted.is_empty() && !exiting {
        return Ok(());
    }
    if exiting {
        #[cfg(test)]
        if crate::runtime::test_seams::retirement_timeout_forced(tree_id) {
            tracing::error!(
                target: "plak.topology",
                "child retirement exceeded the run-exit bound after the stop was recorded"
            );
            return Ok(());
        }
        let retired = tokio::time::timeout(
            crate::topology_runtime::HOST_SHUTDOWN_RELEASE_TIMEOUT,
            async {
                let join = runtime.begin_cancel_live_many(interrupted).await;
                let report = join.await;
                runtime.driver().join_retirement_tasks().await?;
                Ok::<_, String>(report)
            },
        )
        .await;
        let report = match retired {
            Ok(Ok(report)) => report,
            Ok(Err(error)) => return Err(error),
            // The tree's stop records are already durable. A child that is
            // slow to retire must not turn that into a failed stop.
            Err(_) => {
                tracing::error!(
                    target: "plak.topology",
                    "child retirement exceeded the run-exit bound after the stop was recorded"
                );
                return Ok(());
            }
        };
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

/// Runs `work` on `tree_id`'s live hub, or on its persisted rows and saves
/// them when no hub holds it. `None` when the tree has no rows.
async fn on_tree<R: Send + 'static>(
    rows: &dyn TreeRows,
    tree_id: &str,
    epoch_advanced: bool,
    work: impl FnOnce(&mut ProductAgentTree) -> Result<R, String> + Send + 'static,
) -> Result<Option<R>, String> {
    if let Some(runtime) = lookup(tree_id) {
        return runtime
            .mutate_then_emit(epoch_advanced, work)
            .await
            .map(Some);
    }
    let Some(mut tree) = rows.tree(tree_id)? else {
        return Ok(None);
    };
    let answer = work(&mut tree)?;
    rows.save_tree(&mut tree).map_err(|error| error.message())?;
    Ok(Some(answer))
}

/// Whether `tree_id` has rows: a live hub, or a persisted tree.
///
/// # Errors
/// The rows could not be read.
pub(crate) fn tree_has_rows(rows: &dyn TreeRows, tree_id: &str) -> Result<bool, String> {
    if lookup(tree_id).is_some() {
        return Ok(true);
    }
    rows.tree_exists(tree_id)
}

/// The current epoch of `tree_id`, from its hub or its persisted rows.
///
/// # Errors
/// The rows could not be read.
pub(crate) fn current_epoch_of(
    rows: &dyn TreeRows,
    tree_id: &str,
) -> Result<Option<OrchestrationSnapshot>, String> {
    if let Some(runtime) = lookup(tree_id) {
        return runtime
            .read(|tree| tree.current_epoch().clone())
            .map(Some)
            .map_err(|error| error.message);
    }
    Ok(rows.tree(tree_id)?.map(|tree| tree.current_epoch().clone()))
}

/// Step 2 of a profile switch on a tree with rows: the successor epoch's
/// exact bytes, recorded before the publication that compiles against them.
///
/// # Errors
/// The tree refuses the epoch (a root spawn mid-reservation, the epoch
/// bound, another pending switch) or cannot be written.
pub(crate) async fn record_switch_epoch(
    rows: &dyn TreeRows,
    tree_id: &str,
    workflow_id: &str,
    snapshot: OrchestrationSnapshot,
) -> Result<(), String> {
    let workflow = workflow_id.to_owned();
    on_tree(rows, tree_id, false, move |tree| {
        tree.record_epoch_pending(&workflow, &snapshot)
            .map(|_| ())
            .map_err(|error| error.message)
    })
    .await?
    .ok_or_else(|| "the switch's tree has no rows".to_owned())
}

/// Step 5 of a profile switch: appends the epoch `workflow_id` recorded,
/// once its successor is published. `None` when nothing was pending.
///
/// # Errors
/// The tree refuses the append or cannot be written.
pub(crate) async fn settle_switch_epoch(
    rows: &dyn TreeRows,
    tree_id: &str,
    workflow_id: &str,
) -> Result<Option<u64>, String> {
    let workflow = workflow_id.to_owned();
    Ok(on_tree(rows, tree_id, true, move |tree| {
        tree.settle_epoch_pending(&workflow)
            .map_err(|error| error.message)
    })
    .await?
    .flatten())
}

/// Withdraws `workflow_id`'s pending epoch, whose publication definitely did
/// not land.
///
/// # Errors
/// The tree cannot be written.
pub(crate) async fn withdraw_switch_epoch(
    rows: &dyn TreeRows,
    tree_id: &str,
    workflow_id: &str,
) -> Result<(), String> {
    let workflow = workflow_id.to_owned();
    on_tree(rows, tree_id, false, move |tree| {
        Ok(tree.withdraw_epoch_pending(&workflow))
    })
    .await
    .map(|_| ())
}

/// Withdraws every pending epoch left by a switch that did not publish onto
/// the head now open: the head holds its lease, so no switch from it is in
/// flight. Returns the withdrawn workflows.
///
/// # Errors
/// The tree cannot be read or written.
pub async fn withdraw_stale_switch_epochs(
    rows: &dyn TreeRows,
    tree_id: &str,
) -> Result<Vec<String>, String> {
    let stale = match lookup(tree_id) {
        Some(runtime) => runtime
            .read(|tree| tree.epoch_pending().map_err(|error| error.message))
            .map_err(|error| error.message)??,
        None => match rows.tree(tree_id)? {
            Some(tree) => tree.epoch_pending().map_err(|error| error.message)?,
            None => return Ok(Vec::new()),
        },
    };
    if stale.is_empty() {
        return Ok(Vec::new());
    }
    Ok(on_tree(rows, tree_id, false, |tree| {
        tree.withdraw_epoch_pending_except(None)
            .map_err(|error| error.message)
    })
    .await?
    .unwrap_or_default())
}

/// Before `session`'s root opens: when it is a switch successor whose tree
/// still records its epoch as pending (a crash after the publication), the
/// tree takes that epoch from the recorded bytes, so the successor compiles
/// against exactly what it was published with. A tree without rows, and a
/// session no switch published, change nothing.
///
/// # Errors
/// The tree cannot be read, refuses the append, or cannot be written.
pub async fn settle_switch_epoch_on_open(
    rows: &dyn TreeRows,
    tree_id: &str,
    session: &SessionRecord,
) -> Result<(), String> {
    let Some(workflow) = session.source_workflow_operation_id.as_deref() else {
        return Ok(());
    };
    if !tree_has_rows(rows, tree_id)? {
        return Ok(());
    }
    settle_switch_epoch(rows, tree_id, workflow).await?;
    // A switch that kept the roster moves only the tree's view of the root
    // binding; a crash before that view moved leaves the predecessor's.
    let (Some(model), true) = (
        session.model.as_deref(),
        workflow.starts_with("plak:switch:"),
    ) else {
        return Ok(());
    };
    let root_view = |tree: &ProductAgentTree| {
        tree.node(&tree.root_agent_id).ok().map(|root| {
            (
                root.account.clone(),
                root.tuple_id.clone(),
                root.effort.clone(),
            )
        })
    };
    let current = match lookup(tree_id) {
        Some(runtime) => runtime.read(root_view).map_err(|error| error.message)?,
        None => rows.tree(tree_id)?.as_ref().and_then(root_view),
    };
    let account = session.account.as_deref().unwrap_or_default();
    let wanted = (account.to_owned(), model.to_owned(), session.effort.clone());
    if current.is_none_or(|current| {
        (account.is_empty() || current.0 == wanted.0)
            && current.1 == wanted.1
            && current.2 == wanted.2
    }) {
        return Ok(());
    }
    record_root_bind_view(rows, tree_id, account, model, session.effort.as_deref()).await
}

/// Moves the tree's view of the root binding to the line head's, with no
/// epoch: a switch whose roster is unchanged.
///
/// # Errors
/// The tree cannot be written.
pub(crate) async fn record_root_bind_view(
    rows: &dyn TreeRows,
    tree_id: &str,
    account: &str,
    tuple_id: &str,
    effort: Option<&str>,
) -> Result<(), String> {
    let (account, tuple_id, effort) = (
        account.to_owned(),
        tuple_id.to_owned(),
        effort.map(str::to_owned),
    );
    on_tree(rows, tree_id, false, move |tree| {
        tree.record_root_bind(&account, &tuple_id, effort.as_deref());
        Ok(())
    })
    .await
    .map(|_| ())
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
    rows.save_tree(&mut tree).map_err(|error| error.message())?;
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

    fn save_tree(&self, tree: &mut ProductAgentTree) -> Result<(), TreeWriteError> {
        let store = self.trees().map_err(|message| {
            crate::agent_tree::TopologyError::new(
                crate::agent_tree_protocol::TopologyErrorCode::OperationRecoveryRequired,
                message,
            )
        })?;
        let acknowledged = store.save_in_custody(tree)?;
        tree.acknowledge_revision(acknowledged);
        Ok(())
    }

    fn delete_tree(&self, tree_id: &str, expected_revision: u64) -> Result<(), String> {
        self.trees()?
            .delete_in_custody(tree_id, expected_revision)
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
