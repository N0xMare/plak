//! Process-local topology hub: one working copy and live-child map per tree.
//!
//! SQLite remains durable authority. The hub is the in-memory working copy of
//! a tree this process holds in custody (spec 03 §14). Catalog compile looks
//! the hub up; a hub hit opens no store. Any node may bring the hub up from
//! the tree's durable rows; only a root attach may bootstrap a new tree. Hub
//! lifetime follows custody: it lives while a root is attached or resident or
//! a child is live, and ends when the whole tree is dormant or its host shuts
//! down.

mod startup;
pub use startup::{ChildStartup, ChildStartupStage, STARTUP_SLOT_OCCUPIED};

mod registry;
mod runtime;
mod step_hold;
pub use step_hold::{TreeStepPause, TreeStepSnapshot};
mod shutdown;
use shutdown::shutdown_runtime;
pub use shutdown::{
    abandon_live_agents, release_root_lease, release_root_lease_for_handoff,
    shut_down_handed_off_trees,
};

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::FutureExt;

use plakous::agent::{Agent, CommandId, TurnControl, TurnResultHandle};
use plakous::model::TurnId;
use plakous::tools::{ProbeError, WorkspaceProbe};

use crate::tools::TreeDriver;
use crate::tree::{ProductAgentTree, TopologyObserver, TreeStore};
use crate::tree::{TreeRuntimeError, TreeRuntimeResult};
use plakous_workspace::occupancy_vcs::VcsOps;

/// Whether this catalog compile may create the process hub for a missing root.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TopologyHubAttach {
    /// Child spawn, mail reopen, compact, and rebind: lookup only.
    LookupOnly,
    /// Root `build_or_open` may load-or-bootstrap and insert the hub once.
    AttachRoot,
    /// Exact-reopen verification: the catalog compiles over a detached copy
    /// of the tree (this process's attached one, or the persisted rows). It
    /// never inserts a hub, takes a lease, folds startup, settles effects or
    /// saves, never bootstraps a tree, and binds nothing to a live driver.
    Inspect,
}

/// What cancelling one live child turn owes the hub.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct LiveRelease {
    /// Feed records and bounded reads loaded after release. Only this pass
    /// derives the released locator; the returned locator is a construction result.
    pub feed: Option<crate::tree::TreeFeed>,
    /// Construction result of release, never authority for the node's view.
    /// A release without feed evidence cannot move that view.
    pub locator: Option<String>,
    /// The first failure of the cancel-then-release pair. The cancel's message
    /// wins: a cancel that did not settle is why the release is not the
    /// interesting half.
    pub failure: Option<String>,
}

/// A live child's answer to one operator decision.
pub type LiveDecision = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

/// One live child turn, as the hub sees it.
///
/// The hub owns *whether* a turn is live and *when* it ends, and holds the one
/// lock that decides it. It owns nothing about what a session is: every method
/// here is the product's, and the product's implementation is the only thing
/// that knows there is a session behind the handle at all.
///
/// The two consuming methods are consuming because a turn is cancelled once
/// and a session is released once. Follow-up returns an owned operation so
/// delivery never retains the table lock. Its per-child barrier orders queued
/// result registration before terminal handoff, restart, and release.
pub trait LiveChildHandle: Send + Sync {
    /// The host owns this type; the hub never inspects it.
    fn host_observation(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        None
    }
    /// Runtime capability retained for lease-only process-exit release.
    fn agent(&self) -> Option<Agent>;
    /// Content-free exact-route readiness; neutral handles have no probe.
    fn reachability_probe(&self) -> Option<plakous::model::ReachabilityProbe> {
        None
    }
    /// The turn this handle is running.
    fn turn_id(&self) -> TurnId;

    /// A clone of the running turn's control.
    fn turn_control(&self) -> TurnControl;

    /// Feed evidence for a newly admitted turn. A neutral handle without a
    /// session feed cannot publish turn views from its construction identity.
    fn feed(
        &self,
        _cursor: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<crate::tree::TreeFeed>, String>> + Send>> {
        Box::pin(std::future::ready(Ok(None)))
    }

    /// Follows up on the running turn. The owned future reserves this child
    /// from construction; await or drop it before re-acquiring the live table.
    fn follow_up(
        &self,
        command_id: CommandId,
        body: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

    /// Orders in-flight follow-up admission before terminal handoff.
    fn follow_up_barrier(&self) -> Option<Arc<tokio::sync::RwLock<()>>>;

    /// Answers the operator decision `decision` names, when this child's
    /// running turn is the node that holds it: the child is that
    /// decision's single decider. `None` when the child does not hold it.
    fn decide(&self, _decision: &str, _allow: bool) -> Option<LiveDecision> {
        None
    }

    /// The follow-up turn admitted behind the turn that just ended, if any.
    /// It is now the session's active turn: the handle rebinds to it so the
    /// host observes it instead of letting it run unwatched.
    fn take_follow_up(&mut self) -> Option<TurnResultHandle> {
        None
    }

    /// Starts a fresh turn on this child's session and rebinds the handle to
    /// it, so the table names the turn that is actually running. The driver
    /// lets the host settle duplicates through the tree's feed projector.
    /// The caller drains delivery outside the map and passes the exclusive
    /// reservation, retained through rebinding; handles without a barrier pass None.
    fn restart(
        &mut self,
        driver: TreeDriver,
        command_id: CommandId,
        prompt: String,
        _delivery: Option<tokio::sync::OwnedRwLockWriteGuard<()>>,
    ) -> Pin<Box<dyn Future<Output = Result<TurnResultHandle, String>> + Send + '_>>;

    /// Releases the session without cancelling, reporting the locator its
    /// released head sits at. What a hub shutdown owes every turn it drops,
    /// and what a finished child owes the node it advances.
    fn release(
        self: Box<Self>,
        driver: TreeDriver,
    ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>>;

    /// Cancels the turn, waits for it to settle within the host bound,
    /// releases the session, and reports the released head and the first
    /// failure. Never fails as a whole: a cancel that did not settle and a
    /// release that did are both facts the hub records.
    /// When supplied, `dispatched` is sent only after issuing the exact turn's
    /// cancellation, not when constructing or first polling this future.
    fn cancel_and_release(
        self: Box<Self>,
        driver: TreeDriver,
        dispatched: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>>;
}

/// One child node's residency on this tree (design section 4.3): a node
/// with no entry is dormant.
enum Residency {
    /// A startup owns the slot until its turn is acknowledged or released.
    Opening(Box<ChildStartup>),
    /// A live turn; only a resident child takes new work.
    Resident(Box<dyn LiveChildHandle>),
    /// The turn's release is in flight; the slot stays taken until it
    /// returns.
    Releasing(Box<Retirement>),
}

/// One tree's child nodes that are not dormant, one residency each. The
/// map and its lock are the hub's, because the admission decision and the
/// shutdown drain must be one lock; what a resident entry holds is the
/// product's.
#[derive(Default)]
pub struct LiveChildTable {
    nodes: HashMap<String, Residency>,
    retirement_recovery: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Signalled whenever a child leaves the table or a resident root goes:
    /// the hub then checks whether the whole tree is dormant.
    quiet: Arc<tokio::sync::Notify>,
}

impl LiveChildTable {
    /// Whether every child node is dormant.
    fn is_quiet(&self) -> bool {
        self.nodes.is_empty()
    }
}

#[derive(Clone)]
struct Retirement {
    agent: Option<Agent>,
    turn: TurnId,
    expected_attempt: Option<crate::protocol::AttemptId>,
    recovery: Option<Arc<dyn Fn() + Send + Sync>>,
    dispatched: futures_util::future::Shared<futures_util::future::BoxFuture<'static, ()>>,
    completion:
        futures_util::future::Shared<futures_util::future::BoxFuture<'static, Result<(), String>>>,
    abort: crate::task_set::TaskAbortHandle,
}

/// Routes an operator decision to the live child that holds it, and
/// answers what that child's single decider said. `None` when no live
/// child holds `decision`.
pub async fn decide_live(
    live: &Arc<tokio::sync::Mutex<LiveChildTable>>,
    decision: &str,
    allow: bool,
) -> Option<Result<(), String>> {
    let answer = {
        let live = live.lock().await;
        live.residents()
            .find_map(|(_, child)| child.decide(decision, allow))
    };
    Some(answer?.await)
}

/// Releases an exact child turn, or joins its existing retirement. A failed
/// retirement reserves the slot only until its release owner returns.
pub async fn retire_live(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<LiveChildTable>>,
    agent_id: &str,
    expected: Option<TurnId>,
    cancel: bool,
) -> Result<LiveCancelOutcome, LiveCancelError> {
    begin_retire_live(driver, live, agent_id, expected, cancel)
        .await
        .join()
        .await
}

/// Waits, within a retirement's own release deadline, for a retirement of
/// `agent_id`'s turn `turn` that is already under way, and starts none: a
/// turn the host's release cancelled ends while that release still
/// publishes. A turn waiter that waits here folds the turn's end only once
/// the release page is folded, so which of the two runs first never decides
/// the tree's writes.
pub async fn join_release_in_flight(
    live: &Arc<tokio::sync::Mutex<LiveChildTable>>,
    agent_id: &str,
    turn: TurnId,
) {
    let completion = {
        let live = live.lock().await;
        live.releasing(agent_id)
            .filter(|retirement| retirement.turn == turn)
            .map(|retirement| retirement.completion.clone())
    };
    if let Some(completion) = completion {
        let _ = tokio::time::timeout(HOST_RETIRE_JOIN_TIMEOUT, completion).await;
    }
}

enum RetirementJoin {
    Startup(ChildStartup),
    Live {
        retirement: Retirement,
        deadline: tokio::time::Instant,
    },
    NotLive,
}

async fn begin_retire_live(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<LiveChildTable>>,
    agent_id: &str,
    expected: Option<TurnId>,
    cancel: bool,
) -> RetirementJoin {
    let deadline = tokio::time::Instant::now() + HOST_RETIRE_JOIN_TIMEOUT;
    let startup = {
        let live = live.lock().await;
        let startup = expected
            .is_none()
            .then(|| live.opening(agent_id).cloned())
            .flatten();
        if let Some(startup) = &startup {
            startup.request_cancel();
        }
        startup
    };
    if let Some(startup) = startup {
        return RetirementJoin::Startup(startup);
    }
    let retirement =
        live.lock()
            .await
            .start_retire(live.clone(), driver.clone(), agent_id, expected, cancel);
    if let Some(retirement) = &retirement {
        retirement.dispatched.clone().await;
    }
    retirement.map_or(RetirementJoin::NotLive, |retirement| RetirementJoin::Live {
        retirement,
        deadline,
    })
}

impl RetirementJoin {
    async fn join_owned(self) -> Result<LiveCancelOutcome, LiveCancelError> {
        match self {
            Self::Startup(startup) => startup.join().await,
            Self::Live { retirement, .. } => retirement
                .completion
                .await
                .map(|()| LiveCancelOutcome::Cancelled)
                .map_err(LiveCancelError::OwnerFailure),
            Self::NotLive => Ok(LiveCancelOutcome::NotLive),
        }
    }

    async fn join(self) -> Result<LiveCancelOutcome, LiveCancelError> {
        match self {
            Self::Startup(startup) => startup.join_cancel().await,
            Self::Live {
                retirement,
                deadline,
            } => {
                if let Ok(result) = tokio::time::timeout_at(deadline, retirement.completion).await {
                    result
                        .map(|()| LiveCancelOutcome::Cancelled)
                        .map_err(LiveCancelError::OwnerFailure)
                } else {
                    if let Some(recovery) = retirement.recovery {
                        recovery();
                    }
                    Err(LiveCancelError::JoinExpired)
                }
            }
            Self::NotLive => Ok(LiveCancelOutcome::NotLive),
        }
    }
}

async fn publish_release(
    driver: &TreeDriver,
    agent_id: &str,
    release: LiveRelease,
) -> Result<(), String> {
    let feed = release.feed;
    let publication = async {
        let writer = driver.clone();
        let agent = agent_id.to_owned();
        let (result, effects) = driver
            .mutate(move |tree| {
                if tree.revision != tree.stored_revision {
                    writer.save(tree).map_err(|error| error.message)?;
                }
                // A release page that would only move the feed cursor (the
                // turn waiter already folded everything else) is left for the
                // next open to read again: whether the waiter's fold beat the
                // session's release is timing, and the rows must not say.
                let feed = feed.filter(|feed| !moves_only_cursor(tree, feed, &agent));
                writer
                    .fold_and_commit(tree, |tree| {
                        if let Some(feed) = feed {
                            feed.apply(tree, &agent)?;
                        }
                        Ok(())
                    })
                    .map_err(|error| error.message)
            })
            .await
            .map_err(|error| error.message)?;
        result?;
        driver
            .settle(effects)
            .await
            .map(|_| ())
            .map_err(|error| error.message)
    }
    .await;
    match (release.failure, publication) {
        (Some(failure), Err(publication)) => Err(format!(
            "{failure}; locator publication failed: {publication}"
        )),
        (Some(failure), Ok(())) => Err(failure),
        (None, publication) => publication,
    }
}

/// Whether applying `feed` would change nothing of `agent_id`'s node,
/// attempt or commands, only its cursor. (A release page rereads the
/// terminal the turn waiter already applied, so the projection may report
/// it again; the rows say whether anything is new.)
fn moves_only_cursor(
    tree: &ProductAgentTree,
    feed: &crate::tree::TreeFeed,
    agent_id: &str,
) -> bool {
    let rows = |tree: &ProductAgentTree| {
        (
            tree.node(agent_id).ok().cloned(),
            tree.attempt(agent_id).cloned(),
            tree.commands()
                .values()
                .filter(|command| command.recipient == agent_id)
                .cloned()
                .collect::<Vec<_>>(),
        )
    };
    let mut probe = tree.clone();
    let Ok(projection) = feed.apply(&mut probe, agent_id) else {
        return false;
    };
    projection.terminal_usages.is_empty()
        && projection.conflicts.is_empty()
        && projection.refused_intents.is_empty()
        && rows(&probe) == rows(tree)
}

fn retirement_needs_recovery(driver: &TreeDriver, agent_id: &str) -> bool {
    driver
        .read(|tree| {
            tree.node(agent_id)
                .is_ok_and(|node| node.status == crate::protocol::NodeStatus::RecoveryRequired)
                || matches!(
                    tree.recovery_need(agent_id),
                    Ok(crate::tree::RecoveryNeed::Session { .. })
                )
        })
        .unwrap_or(true)
}

enum RetirementFaultSource {
    Owner,
    OwnerAlreadyReported,
    Joiner,
}

async fn record_retirement_fault(
    driver: &TreeDriver,
    agent_id: &str,
    expected: Option<&crate::protocol::AttemptId>,
    source: RetirementFaultSource,
    error: &str,
) {
    tracing::error!(target: "plak.topology", agent_id, error, "child retirement failed");
    let writer = driver.clone();
    let (agent_id_owned, expected, error_owned) =
        (agent_id.to_owned(), expected.cloned(), error.to_owned());
    // Fault reporting must not unwind out of the release it is fencing.
    let recorded = std::panic::AssertUnwindSafe(driver.mutate_and_settle(move |tree| {
        let (agent_id, expected, error, driver) = (
            agent_id_owned.as_str(),
            expected.as_ref(),
            error_owned.as_str(),
            &writer,
        );
        // Only the live release owner can fence a committed terminal result:
        // terminal observation itself does not establish successful release.
        // A different current attempt can only be an unstarted followup:
        // the retiring slot still excludes startup. Fence it rather than
        // allowing rollback to erase the owner's release failure.
        let fenced = match source {
            RetirementFaultSource::Owner | RetirementFaultSource::OwnerAlreadyReported => tree
                .record_host_retirement(agent_id, error.to_owned())
                .and_then(|value| driver.apply_recorded(tree, agent_id).map(|()| value))
                .map(|()| true),
            RetirementFaultSource::Joiner => tree
                .record_host_retirement_for_attempt(agent_id, expected, error.to_owned())
                .and_then(|value| driver.apply_recorded(tree, agent_id).map(|()| value)),
        };
        if let Err(publish) = fenced {
            tree.record_fault(agent_id, error);
            tracing::error!(target: "plak.topology", agent_id, error = %publish.message,
                "child retirement could not be fenced");
        }
        if let Err(error) = driver.save(tree) {
            tracing::error!(target: "plak.topology", agent_id, error = %error.message,
                "child retirement fault could not be persisted");
        }
        if matches!(source, RetirementFaultSource::Owner) {
            tree.try_emit_tree_node_detached(
                driver.observer(),
                agent_id,
                crate::tree::NodeDetach::RecoveryRequired,
            );
            let closed = tree
                .attempt(agent_id)
                .is_some_and(|attempt| attempt.status == crate::protocol::AttemptStatus::Closed);
            let prefix = if closed {
                "close applied; live cancel failed"
            } else {
                "child retirement failed"
            };
            let _ = driver.retirement_faults.send((
                agent_id.to_owned(),
                format!("{prefix} for {agent_id}: {error}; recovery required"),
            ));
        }
    }))
    .catch_unwind()
    .await;
    match recorded {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            tracing::error!(target: "plak.topology", agent_id, error = %error.message,
            "child retirement fault could not be recorded");
        }
        Err(_) => tracing::error!(target: "plak.topology", agent_id,
            "child retirement fault publication panicked"),
    }
}

/// Holds a child's recycling mark; dropping it clears the mark.
pub struct RecyclingGuard {
    runtime: Arc<ProductTopologyRuntime>,
    agent_id: String,
}

impl Drop for RecyclingGuard {
    fn drop(&mut self) {
        if let Ok(mut recycling) = self.runtime.recycling.lock() {
            recycling.remove(&self.agent_id);
        }
    }
}

/// One `recover_node` run's hold on a child; dropping it releases the child.
pub struct RecoveryClaim {
    runtime: Arc<ProductTopologyRuntime>,
    agent_id: String,
}

impl Drop for RecoveryClaim {
    fn drop(&mut self) {
        if let Ok(mut recovering) = self.runtime.recovering.lock() {
            recovering.remove(&self.agent_id);
        }
    }
}

/// Why [`ProductTopologyRuntime::claim_recovery`] refused a child.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryClaimRefusal {
    /// A reversible host step pause, not a recovery or runtime fault.
    StepHeld,
    /// Its turn waiter is recycling it: this process owns it.
    Recycling,
    /// Another `recover_node` run holds it.
    Recovering,
}

/// One attached tree and everything a host process holds around it: the tree
/// itself, its driver, and the live child turns running under it.
///
/// The hub is what a rejoining process looks a tree up in; `lookup` finds it
/// by tree id, so a second attach shares this one rather than opening the
/// store twice.
pub struct ProductTopologyRuntime {
    /// Live child turns, keyed by agent id. Its lock is separate from the
    /// tree's on purpose: admission and the shutdown drain must be one lock,
    /// and neither may be taken while the tree's is held.
    pub live: Arc<tokio::sync::Mutex<LiveChildTable>>,
    /// The tree with its store and VCS port: every mutation, effect, and
    /// publish for this tree goes through it.
    driver: TreeDriver,
    closed: AtomicBool,
    step_paused: AtomicBool,
    step_released: tokio::sync::Notify,
    step_recovery_due: AtomicBool,
    /// Children whose fenced turn's owner is releasing and reopening their
    /// session. Recovery treats them as owned.
    recycling: Mutex<HashSet<String>>,
    /// Children a `recover_node` run holds, so two runs never reopen one
    /// child at once.
    recovering: Mutex<HashSet<String>>,
    /// Running `recover_node` runs over this tree's children. Shutdown waits
    /// for, or aborts, every one before it releases leases.
    recoveries: Mutex<Vec<crate::task_set::TaskHandle<()>>>,
    rejoin_spec: Mutex<Option<Arc<dyn Any + Send + Sync>>>,
    observer: Option<Arc<dyn TopologyObserver>>,
    /// Root runtimes built against this tree without a session attachment
    /// (a rebind's successor before it is opened): each keeps the tree
    /// resident while it lives.
    residents: std::sync::atomic::AtomicUsize,
    /// The live table's dormancy signal.
    quiet: Arc<tokio::sync::Notify>,
    /// Where this custody take's custody-attach scan stands.
    attach_scan: std::sync::atomic::AtomicU8,
    /// The executor of the root session whose lease brought the tree into
    /// use (the host's own, which outlives a short-lived build that leases
    /// the tree beside it), or the one the tree was opened on before any.
    executor: Mutex<Option<tokio::runtime::Handle>>,
    /// Whether a child held after a loss continues by itself (the root's
    /// `resume_after_loss = "continue"`), set by the attached root.
    continue_after_loss: AtomicBool,
}

/// The custody-attach scan (design section 6.6) is owed: no root has
/// attached since this process took the tree's custody.
const ATTACH_SCAN_DUE: u8 = 0;
/// The scan's rejoin ran; its recovery pass is owed to the root's open.
const ATTACH_SCAN_PASS_OWED: u8 = 1;
/// The scan ran.
const ATTACH_SCAN_DONE: u8 = 2;

/// A root runtime's residency in its tree's hub: while it lives the tree is
/// not dormant, so its custody and children outlive the release of every
/// root session attachment (spec 03 §14).
pub struct HubResidency(Arc<ProductTopologyRuntime>);

impl Drop for HubResidency {
    fn drop(&mut self) {
        self.0.residents.fetch_sub(1, Ordering::AcqRel);
        self.0.note_quiet();
    }
}

/// What one [`ProductTopologyRuntime::cancel_live`] did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LiveCancelOutcome {
    /// A live turn was found, cancelled, and released.
    Cancelled,
    /// The agent held no live turn; nothing was stopped, and nothing failed.
    NotLive,
}

/// A bounded join refusal is not owner-confirmed cleanup failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LiveCancelError {
    /// The release owner is still running and owns deadline fault publication.
    JoinExpired,
    /// The owner returned a cleanup or publication failure.
    OwnerFailure(String),
}

impl std::fmt::Display for LiveCancelError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::JoinExpired => formatter.write_str(RETIRE_JOIN_EXPIRED),
            Self::OwnerFailure(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for LiveCancelError {}

/// Owner-confirmed retirement failure, scoped before awaiting cancellation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveRetirementFailure {
    /// Tree owning the original release; child ids can repeat in another tree.
    pub tree_id: String,
    /// Agent whose release failed.
    pub agent_id: String,
    /// Attempt owning the release, or none for an unattached root.
    pub expected_attempt_id: Option<crate::protocol::AttemptId>,
    /// Exact retiring turn, absent only for a startup without an admitted turn.
    pub expected_turn_id: Option<TurnId>,
    /// The owner's cleanup failure.
    pub message: String,
}

impl LiveRetirementFailure {
    pub(crate) fn matches(&self, tree: &ProductAgentTree) -> bool {
        tree.tree_id == self.tree_id
            && tree
                .attempt(&self.agent_id)
                .map(|attempt| &attempt.attempt_id)
                == self.expected_attempt_id.as_ref()
            && !self.expected_turn_id.is_some_and(|expected| {
                tree.attempt(&self.agent_id)
                    .and_then(|attempt| attempt.turn_id.as_deref())
                    .is_some_and(|turn| turn != expected.to_string())
            })
    }
}

/// What one batch cancel did, agent by agent.
///
/// Four buckets rather than a `Result`: not-live differs from cancelled,
/// and a bounded joiner refusal does not authorize another persisted fault.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LiveCancelReport {
    /// Agents whose live turn was cancelled and released.
    pub cancelled: Vec<String>,
    /// Agents that held no live turn.
    pub not_live: Vec<String>,
    /// Agents whose cancel or release failed; these fail closed.
    pub failed: Vec<String>,
    /// Joiners whose bounds expired, without a second fault publication.
    pub joiner_expired: Vec<String>,
    /// Exact owner-confirmed failures, never joiner deadline expiry.
    pub owner_failures: Vec<LiveRetirementFailure>,
    /// The first failure's message, when anything failed.
    pub first_failure: Option<String>,
}

impl LiveCancelReport {
    /// Whether every agent was either cancelled or simply not live.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.failed.is_empty() && self.joiner_expired.is_empty()
    }
}

/// How a root session gives back its attachment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RootRelease {
    /// A rebind hands its tree to its successor: the tree stays up while any
    /// of its nodes is live or resident.
    Detach,
    /// The host that attached it is shutting the session down: the last
    /// attachment runs the tree's joined shutdown.
    Shutdown,
}

struct HubEntry {
    runtime: Arc<ProductTopologyRuntime>,
    /// Root session attachments: each `AttachRoot` takes one and its session
    /// release gives it back.
    attachments: u32,
    /// A rebind's hand-off gave back the last attachment while the tree was
    /// not dormant, so the hub keeps it until it is, or until the host quits.
    handed_off: bool,
}

type HubEntries = Arc<Mutex<HashMap<String, HubEntry>>>;

/// The process's attached trees: one working copy and live-child map per
/// tree, keyed by tree id. Owned by whoever attaches roots — nothing here is
/// a static — and looked up by every child compile, mail reopen, and rebind.
///
/// A tree's entry lives with its custody (spec 03 §14): while a root session
/// is attached, a root runtime is resident, or a child turn is live,
/// starting or retiring. It ends when the whole tree is dormant, or when the
/// host that attached it shuts down; a rebind's hand-off alone never ends it.
///
/// The hub mutex is held only to read or edit the map; no tree mutex is ever
/// taken under it, so a tree-holding caller may look the hub up.
#[derive(Default)]
pub struct TopologyHub {
    entries: HubEntries,
    /// Held shared by each dormancy watcher through its own shutdown of a
    /// tree it removed, so the host's quit can join that shutdown too.
    ending: Arc<tokio::sync::RwLock<()>>,
}

impl TopologyHub {
    /// An empty hub. The process uses one of these; tests build their own.
    pub fn new() -> Self {
        Self::default()
    }

    fn entries(&self) -> TreeRuntimeResult<std::sync::MutexGuard<'_, HashMap<String, HubEntry>>> {
        self.entries
            .lock()
            .map_err(|_| TreeRuntimeError::new("topology hub lock is poisoned"))
    }

    /// The attached runtime for this tree, when this hub holds one.
    pub fn lookup(&self, tree_id: &str) -> Option<Arc<ProductTopologyRuntime>> {
        let entries = self.entries.lock().ok()?;
        entries.get(tree_id).map(|entry| Arc::clone(&entry.runtime))
    }

    fn runtimes(&self) -> Vec<Arc<ProductTopologyRuntime>> {
        self.entries
            .lock()
            .map(|entries| {
                entries
                    .values()
                    .map(|entry| Arc::clone(&entry.runtime))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Inserts a working copy over `store` and `vcs`, or returns the
    /// occupant if this tree is already attached.
    pub async fn insert_root(
        &self,
        tree_id: impl Into<String>,
        tree: ProductAgentTree,
        store: Arc<dyn TreeStore>,
        vcs: Arc<dyn VcsOps>,
        observer: Option<Arc<dyn TopologyObserver>>,
    ) -> TreeRuntimeResult<Arc<ProductTopologyRuntime>> {
        let tree_id = tree_id.into();
        if let Some(entry) = self.entries()?.get(&tree_id) {
            return Ok(Arc::clone(&entry.runtime));
        }
        let runtime = Arc::new(ProductTopologyRuntime::new(tree, store, vcs, observer));
        // Only the custody holder materializes the tree (spec 03 §14).
        if let Some(refusal) = runtime.driver.custody_refusal() {
            return Err(refusal.into());
        }
        let effects = match runtime.driver.reconcile_attachment().await {
            Ok(()) => runtime.driver.fold_startup().await,
            Err(error) => Err(error),
        }
        .map_err(|error| TreeRuntimeError::new(error.message));
        let occupant = {
            let mut entries = self.entries()?;
            if let Some(entry) = entries.get(&tree_id) {
                Some(Arc::clone(&entry.runtime))
            } else {
                if effects.is_ok() {
                    entries.insert(
                        tree_id,
                        HubEntry {
                            runtime: Arc::clone(&runtime),
                            attachments: 0,
                            handed_off: false,
                        },
                    );
                }
                None
            }
        };
        if let Some(occupant) = occupant {
            if let Ok(effects) = effects {
                runtime
                    .driver
                    .settle(effects)
                    .await
                    .map_err(|error| TreeRuntimeError::new(error.message))?;
            }
            return Ok(occupant);
        }
        runtime
            .driver
            .settle(effects?)
            .await
            .map_err(|error| TreeRuntimeError::new(error.message))?;
        Ok(runtime)
    }

    /// Takes one root attachment on an attached tree, so the hub outlives
    /// every root session that is still using it.
    ///
    /// # Errors
    ///
    /// Refuses when no runtime is attached under this id.
    pub fn acquire_root_lease(&self, tree_id: &str) -> TreeRuntimeResult<()> {
        let mut entries = self.entries()?;
        let entry = entries.get_mut(tree_id).ok_or_else(|| {
            TreeRuntimeError::new("topology hub is not attached; cannot acquire a root lease")
        })?;
        if entry.attachments == 0 {
            // The first lease is taken on the attaching root's executor.
            if let Ok(executor) = tokio::runtime::Handle::try_current() {
                entry.runtime.host_on(executor);
            }
        }
        entry.attachments = entry.attachments.saturating_add(1);
        Ok(())
    }

    /// Looks the tree up, or brings it up from the store: any node may load
    /// the tree's durable rows, and only an attaching root may bootstrap a
    /// new tree through `load_or_bootstrap`. `store` is asked for only on
    /// that path: a hub hit opens nothing.
    pub async fn resolve<B, F>(
        &self,
        tree_id: &str,
        is_root: bool,
        attach: TopologyHubAttach,
        store: impl FnOnce() -> TreeRuntimeResult<Arc<dyn TreeStore>>,
        observer: Option<Arc<dyn TopologyObserver>>,
        load_or_bootstrap: B,
    ) -> TreeRuntimeResult<Arc<ProductTopologyRuntime>>
    where
        B: FnOnce(Arc<dyn TreeStore>) -> F,
        F: Future<Output = TreeRuntimeResult<ProductAgentTree>>,
    {
        let runtime = if let Some(runtime) = self.lookup(tree_id) {
            runtime
        } else if attach == TopologyHubAttach::Inspect {
            return Err(TreeRuntimeError::new(
                "topology hub is not attached; an inspection never brings a tree up",
            ));
        } else {
            let store = store()?;
            // Custody is taken before the load or bootstrap, whose save it
            // fences, and kept until the driver holds its own share.
            let custody = AttachCustody {
                hold: Some(
                    crate::tree::acquire_custody(store.custody_port(), tree_id).map_err(
                        |error| TreeRuntimeError::from(crate::tree::TreeStoreError::from(error)),
                    )?,
                ),
                store: Arc::clone(&store),
            };
            let tree = if is_root && attach == TopologyHubAttach::AttachRoot {
                load_or_bootstrap(Arc::clone(&store)).await?
            } else {
                // A child, or a root that is not attaching, brings up only the
                // tree that is already durable; it never bootstraps one.
                let persisted = store.load(tree_id)?.ok_or_else(|| {
                    TreeRuntimeError::new(
                        "topology tree is not in the store; only a root attach bootstraps a tree",
                    )
                })?;
                let tree = ProductAgentTree::reopen(persisted);
                if !tree.epoch_digests_match() {
                    return Err(TreeRuntimeError::new(
                        "orchestration snapshot digest does not match persisted roster; RecoveryRequired",
                    ));
                }
                tree
            };
            let vcs = tree.vcs_port();
            let runtime = self
                .insert_root(tree_id, tree, store, vcs, observer)
                .await?;
            drop(custody);
            runtime
        };
        if attach == TopologyHubAttach::AttachRoot {
            self.acquire_root_lease(tree_id)?;
        }
        Ok(runtime)
    }

    /// Gives back a root session attachment. The last one hands the runtime
    /// back for its joined shutdown when `release` is the host's shutdown,
    /// when that host's shutdown is latched, or when the whole tree is
    /// dormant; otherwise the tree stays in custody and the hub ends it once
    /// it goes dormant.
    async fn release_root_lease(
        &self,
        tree_id: &str,
        release: RootRelease,
    ) -> TreeRuntimeResult<Option<Arc<ProductTopologyRuntime>>> {
        let runtime = {
            let mut entries = self.entries()?;
            let Some(entry) = entries.get_mut(tree_id) else {
                return Ok(None);
            };
            if entry.attachments == 0 {
                return Ok(None);
            }
            entry.attachments -= 1;
            if entry.attachments > 0 {
                return Ok(None);
            }
            Arc::clone(&entry.runtime)
        };
        if release == RootRelease::Detach
            && !runtime.driver.host_shutdown_latched()
            && !runtime.is_dormant().await
        {
            // Root dormancy never ends the tree: its live children and
            // resident roots keep it, and the hub ends it once they are gone.
            if let Some(entry) = self.entries()?.get_mut(tree_id)
                && Arc::ptr_eq(&entry.runtime, &runtime)
            {
                entry.handed_off = true;
            }
            watch_dormancy(
                Arc::downgrade(&self.entries),
                Arc::clone(&self.ending),
                tree_id.to_owned(),
                &runtime,
            );
            return Ok(None);
        }
        Ok(remove_unattached(&self.entries, tree_id, &runtime))
    }

    /// The host's quit: removes every tree a rebind's hand-off left in
    /// custody that no root session holds again and runs each one's joined
    /// shutdown, then joins the shutdowns dormancy watchers already began.
    async fn shut_down_handed_off(&self) -> Result<(), TopologyShutdownError> {
        let runtimes = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let handed_off = entries
                .iter()
                .filter(|(_, entry)| entry.attachments == 0 && entry.handed_off)
                .map(|(tree_id, _)| tree_id.clone())
                .collect::<Vec<_>>();
            handed_off
                .iter()
                .filter_map(|tree_id| entries.remove(tree_id))
                .map(|entry| entry.runtime)
                .collect::<Vec<_>>()
        };
        let mut failure = TopologyShutdownError::default();
        for result in
            futures_util::future::join_all(runtimes.into_iter().map(shutdown_runtime)).await
        {
            if let Err(error) = result {
                failure.child_failures.extend(error.child_failures);
                failure.tree_failure = failure.tree_failure.or(error.tree_failure);
            }
        }
        drop(self.ending.write().await);
        if failure.child_failures.is_empty() && failure.tree_failure.is_none() {
            Ok(())
        } else {
            Err(failure)
        }
    }

    /// Removes a tree regardless of leases (a failed root attach, a test
    /// teardown); `unleased_only` keeps a leased tree in place.
    #[cfg(any(test, feature = "test-support"))]
    fn evict(&self, tree_id: &str, unleased_only: bool) -> Option<Arc<ProductTopologyRuntime>> {
        let mut entries = self.entries.lock().ok()?;
        match entries.get(tree_id) {
            Some(entry) if !unleased_only || entry.attachments == 0 => {
                entries.remove(tree_id).map(|entry| entry.runtime)
            }
            _ => None,
        }
    }
}

/// An attach's own custody share, given back on every exit from the attach:
/// after a failure it is the last share and releases the record, after a
/// success the driver's share keeps the record held.
struct AttachCustody {
    store: Arc<dyn TreeStore>,
    hold: Option<crate::tree::CustodyHold>,
}

impl Drop for AttachCustody {
    fn drop(&mut self) {
        let Some(hold) = self.hold.take() else {
            return;
        };
        if let Err(error) = crate::tree::release_custody_hold(self.store.custody_port(), hold) {
            tracing::error!(
                target: "plak.topology",
                error = %error,
                "agent tree custody was not released after a failed attach; this process keeps it until it exits"
            );
        }
    }
}

fn remove_unattached(
    entries: &Mutex<HashMap<String, HubEntry>>,
    tree_id: &str,
    runtime: &Arc<ProductTopologyRuntime>,
) -> Option<Arc<ProductTopologyRuntime>> {
    let mut entries = entries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match entries.get(tree_id) {
        Some(entry) if Arc::ptr_eq(&entry.runtime, runtime) && entry.attachments == 0 => {
            entries.remove(tree_id).map(|entry| entry.runtime)
        }
        _ => None,
    }
}

/// Ends a tree's hub entry once the whole tree is dormant and no root is
/// attached, armed when the last root detached from a tree that was not:
/// woken whenever a child leaves the live table or a resident root goes, it
/// removes the entry and runs the joined shutdown, after which the driver's
/// last clone gives the tree's custody up. It ends with the entry, or when a
/// root attaches again.
fn watch_dormancy(
    entries: std::sync::Weak<Mutex<HashMap<String, HubEntry>>>,
    ending: Arc<tokio::sync::RwLock<()>>,
    tree_id: String,
    runtime: &Arc<ProductTopologyRuntime>,
) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let quiet = runtime.quiet();
    let runtime = Arc::downgrade(runtime);
    drop(handle.spawn(async move {
        loop {
            quiet.notified().await;
            let (Some(runtime), Some(entries)) = (runtime.upgrade(), entries.upgrade()) else {
                return;
            };
            let attached = entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&tree_id)
                .map(|entry| (Arc::ptr_eq(&entry.runtime, &runtime), entry.attachments));
            if attached != Some((true, 0)) {
                return;
            }
            if !runtime.is_dormant().await {
                continue;
            }
            let _ending = Arc::clone(&ending).read_owned().await;
            if let Some(runtime) = remove_unattached(&entries, &tree_id, &runtime) {
                if let Err(error) = shutdown_runtime(runtime).await {
                    tracing::error!(target: "plak.topology", tree_id = %tree_id, error = %error,
                        "a dormant tree did not shut down cleanly");
                }
                return;
            }
        }
    }));
}

/// The process's one hub, until the product's runtime roots carry it as an
/// `Arc` and hand it to the binder, the navigator, and the rebind flows.
/// Transitional: every free function below is `process_hub().<method>`.
fn process_hub() -> &'static TopologyHub {
    static HUB: OnceLock<TopologyHub> = OnceLock::new();
    HUB.get_or_init(TopologyHub::new)
}

/// The process hub's runtime for this tree, when one is attached. This is how
/// a second attach finds the first instead of opening the store twice.
pub fn lookup(tree_id: &str) -> Option<Arc<ProductTopologyRuntime>> {
    process_hub().lookup(tree_id)
}

/// `path` resolved through `probe`; a path that does not resolve keeps its
/// recorded form. Called outside every tree lock.
fn resolved_or_recorded(probe: &dyn WorkspaceProbe, path: &Path) -> Result<PathBuf, ProbeError> {
    Ok(probe
        .canonicalize(path)?
        .unwrap_or_else(|| path.to_path_buf()))
}

/// Observational Read/Grep stamp for a session workspace root, if a hub tree
/// owns it. Paths are resolved between two short tree reads, never under the
/// tree lock.
///
/// # Errors
/// Returns [`ProbeError::Unavailable`] when the workspace cannot be probed.
pub fn generation_stamp_for_workspace_root(
    runner_id: &str,
    root: &Path,
    probe: &dyn WorkspaceProbe,
) -> Result<Option<Arc<AtomicU64>>, ProbeError> {
    let root = resolved_or_recorded(probe, root)?;
    for runtime in process_hub().runtimes() {
        let Ok(rows) = runtime
            .driver()
            .read(|tree| tree.workspace_paths_on(runner_id))
        else {
            continue;
        };
        let resolved = rows
            .into_iter()
            .map(|(id, path)| Ok((id, resolved_or_recorded(probe, &path)?)))
            .collect::<Result<Vec<_>, ProbeError>>()?;
        if let Ok(Some(stamp)) = runtime
            .driver()
            .read(|tree| tree.generation_counter_for_root(&root, &resolved))
        {
            return Ok(Some(stamp));
        }
    }
    Ok(None)
}

/// Recorded mint base for a copy owned by an attached tree. Paths are
/// resolved after the tree read, never under the tree lock.
///
/// # Errors
/// Returns [`ProbeError::Unavailable`] when the workspace cannot be probed.
pub fn workspace_base_for_root(
    runner_id: &str,
    root: &Path,
    probe: &dyn WorkspaceProbe,
) -> Result<Option<String>, ProbeError> {
    let root = resolved_or_recorded(probe, root)?;
    for runtime in process_hub().runtimes() {
        let Ok(rows) = runtime.driver().read(|tree| {
            tree.workspaces()
                .values()
                .filter(|row| {
                    tree.node(&row.owner_agent_id).is_ok_and(|node| {
                        node.placement.runner_id.as_deref().unwrap_or("local") == runner_id
                    })
                })
                .map(|row| (row.path.clone(), row.base_commit.clone()))
                .collect::<Vec<_>>()
        }) else {
            continue;
        };
        let mut matching = Vec::new();
        for (path, base) in rows {
            let resolved = resolved_or_recorded(probe, &path)?;
            if root == resolved || root.starts_with(resolved) {
                matching.push((path, base));
            }
        }
        if let Some(base) = matching
            .into_iter()
            .max_by_key(|(path, _)| path.components().count())
            .and_then(|(_, base)| base)
        {
            return Ok(Some(base));
        }
    }
    Ok(None)
}

/// Inserts a working copy over an in-memory store, or returns the occupant
/// if this tree is already attached. Test attach only: production roots
/// attach through [`resolve_topology_runtime`] with the product's store.
#[cfg(any(test, feature = "test-support"))]
pub async fn insert_root(
    tree_id: impl Into<String>,
    tree: ProductAgentTree,
    observer: Option<Arc<dyn TopologyObserver>>,
) -> TreeRuntimeResult<Arc<ProductTopologyRuntime>> {
    let vcs = tree.vcs_port();
    process_hub()
        .insert_root(
            tree_id,
            tree,
            Arc::new(crate::tree::MemoryTreeStore::new()),
            vcs,
            observer,
        )
        .await
}

/// Inserts a working copy over an explicit store, or returns the occupant.
#[cfg(any(test, feature = "test-support"))]
pub async fn insert_root_with(
    tree_id: impl Into<String>,
    tree: ProductAgentTree,
    store: Arc<dyn TreeStore>,
    observer: Option<Arc<dyn TopologyObserver>>,
) -> TreeRuntimeResult<Arc<ProductTopologyRuntime>> {
    let vcs = tree.vcs_port();
    process_hub()
        .insert_root(tree_id, tree, store, vcs, observer)
        .await
}

/// Looks up the hub, or load/bootstraps it only for an attaching root, over
/// the store the product hands out for it (asked for on that path alone).
pub async fn resolve_topology_runtime<B, F>(
    tree_id: &str,
    is_root: bool,
    attach: TopologyHubAttach,
    store: impl FnOnce() -> TreeRuntimeResult<Arc<dyn TreeStore>>,
    observer: Option<Arc<dyn TopologyObserver>>,
    load_or_bootstrap: B,
) -> TreeRuntimeResult<Arc<ProductTopologyRuntime>>
where
    B: FnOnce(Arc<dyn TreeStore>) -> F,
    F: Future<Output = TreeRuntimeResult<ProductAgentTree>>,
{
    process_hub()
        .resolve(tree_id, is_root, attach, store, observer, load_or_bootstrap)
        .await
}

/// Failures retained while shutting down an owned tree.
#[derive(Debug, Default)]
pub struct TopologyShutdownError {
    /// Exact agent identities and their retirement failures.
    pub child_failures: Vec<(String, String)>,
    /// Failure to publish or access the tree itself.
    pub tree_failure: Option<String>,
    /// Children whose release the shutdown's bound cut short: their turn was
    /// still running, and their owner kept it. Nothing was recorded for them,
    /// so their next owner decides them (spec 03 section 14.1).
    pub cut_short: Vec<String>,
}

impl TopologyShutdownError {
    /// The children the bound cut short, when that is the whole failure.
    #[must_use]
    pub fn only_cut_short(&self) -> Option<&[String]> {
        (self.tree_failure.is_none()
            && self.child_failures.is_empty()
            && !self.cut_short.is_empty())
        .then_some(self.cut_short.as_slice())
    }
}

impl std::fmt::Display for TopologyShutdownError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "topology shutdown failed")?;
        if let Some(error) = &self.tree_failure {
            write!(f, "; tree: {error}")?;
        }
        for (agent, error) in &self.child_failures {
            write!(f, "; child {agent}: {error}")?;
        }
        for agent in &self.cut_short {
            write!(
                f,
                "; child {agent}: child retirement exceeded the shutdown bound; owner retained"
            )?;
        }
        Ok(())
    }
}
impl std::error::Error for TopologyShutdownError {}

/// Shutdown and headless exit join children within this allowance. Expiry is a
/// bounded refusal, not cancellation of the workers that still own release.
pub const HOST_SHUTDOWN_RELEASE_TIMEOUT: Duration = Duration::from_secs(2);
/// A retirement's release deadline: the cancel settle, then the runtime's
/// store-bounded shutdown. Shorter than that, a slow but healthy release would
/// be fenced as failed and close would refuse.
const HOST_RETIRE_JOIN_TIMEOUT: Duration =
    plakous_host_protocol::limits::HOST_CANCEL_SETTLE_TIMEOUT
        .saturating_add(plakous_host_protocol::limits::HOST_SESSION_RELEASE_TIMEOUT);
const RETIRE_JOIN_EXPIRED: &str = "child retirement exceeded its release deadline; owner retained";
/// After abort, do not join unbounded. A child whose HTTP read ignores cancel
/// must not hold process exit.
const HOST_SHUTDOWN_DETACH_TIMEOUT: Duration = Duration::from_millis(250);

/// Fixtures the crates above this one drive: a unique tree id, an in-memory
/// store, and a bootstrapped root. Behind `test-support`; never compiled into
/// the product.
#[cfg(any(test, feature = "test-support"))]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test fixtures panic on a broken tree rather than typing an error"
)]
pub mod test_support {
    use std::path::PathBuf;
    use std::sync::Arc;

    use crate::protocol::WriterMode;
    use crate::tree::{ProductAgentTree, TreeStore};

    /// A tree id no other fixture in this process will use.
    pub fn unique_tree_id() -> String {
        format!("hub-{}", uuid::Uuid::new_v4().simple())
    }

    /// An in-process store, for a fixture that needs durability but not a
    /// file.
    pub fn memory_store() -> Arc<dyn TreeStore> {
        Arc::new(crate::tree::MemoryTreeStore::new())
    }

    /// A fresh single-lease tree rooted at `root`, with the unit-test
    /// working set already pinned.
    pub fn bootstrap(tree_id: &str, root: PathBuf) -> ProductAgentTree {
        ProductAgentTree::bootstrap_root_for_test(
            tree_id,
            "root",
            "owner",
            root,
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap()
    }

    /// Contract: an attach that the hub can already answer opens no store and
    /// loads no tree — root or child, `AttachRoot` or `LookupOnly`.
    ///
    /// This is the tree side of the product's "the tree store is opened once"
    /// rule, so it lives here as a function rather than as a test name a
    /// consumer has to cite: `plak-host`'s state roots run it too, and the
    /// two crates cannot drift on what a hub hit is allowed to do.
    ///
    /// # Panics
    ///
    /// Panics when a hub hit asks for a store, asks for a tree, or resolves
    /// to a different tree than the one already attached.
    pub async fn hub_hit_opens_no_store(root: PathBuf) {
        use std::sync::atomic::{AtomicBool, Ordering};

        use super::{
            TopologyHubAttach, TreeRuntimeError, force_release, insert_root,
            resolve_topology_runtime,
        };

        let tree_id = unique_tree_id();
        let runtime = insert_root(tree_id.clone(), bootstrap(&tree_id, root), None)
            .await
            .unwrap();
        let opened = AtomicBool::new(false);
        for (is_root, attach) in [
            (true, TopologyHubAttach::AttachRoot),
            (true, TopologyHubAttach::LookupOnly),
            (false, TopologyHubAttach::LookupOnly),
        ] {
            let resolved = resolve_topology_runtime(
                &tree_id,
                is_root,
                attach,
                || {
                    opened.store(true, Ordering::SeqCst);
                    Err(TreeRuntimeError::new("a hub hit must not open a store"))
                },
                None,
                |_| async { Err(TreeRuntimeError::new("a hub hit must not load a tree")) },
            )
            .await
            .unwrap();
            assert!(Arc::ptr_eq(
                runtime.tree_handle_for_test(),
                resolved.tree_handle_for_test()
            ));
        }
        assert!(!opened.load(Ordering::SeqCst));
        force_release(&tree_id).await;
    }
}

/// Removes a hub entry that never acquired a root lease (failed root attach).
#[cfg(any(test, feature = "test-support"))]
pub async fn release_unleased(tree_id: &str) {
    if let Some(runtime) = process_hub().evict(tree_id, true)
        && let Err(error) = shutdown_runtime(runtime).await
    {
        tracing::error!(target: "plak.topology", error = %error, "test hub teardown failed");
    }
}

/// Evicts a tree from the process hub and shuts its runtime down, so one
/// test's hub state cannot leak into the next.
#[cfg(any(test, feature = "test-support"))]
pub async fn force_release(tree_id: &str) {
    if let Some(runtime) = process_hub().evict(tree_id, false)
        && let Err(error) = shutdown_runtime(runtime).await
    {
        tracing::error!(target: "plak.topology", error = %error, "test hub teardown failed");
    }
}

#[cfg(test)]
mod admission_tests;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
