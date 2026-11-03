//! Process-local topology hub: one working copy and live-child map per tree.
//!
//! SQLite remains durable authority. The hub is the in-memory working copy for
//! one root session attach. Catalog compile looks the hub up; it must not
//! `load_tree` on child or mail-reopen catalogs (that would pause the tree and
//! mint a new `Arc`). Hub lifetime is root attach → last root lease release.

mod startup;
pub use startup::{ChildStartup, ChildStartupStage, STARTUP_SLOT_OCCUPIED};

mod registry;
mod runtime;
mod shutdown;
pub use shutdown::{abandon_live_agents, release_root_lease};

#[cfg(any(test, feature = "test-support"))]
use shutdown::shutdown_runtime;

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
use plakous::tools::{LocalWorkspaceProbe, ProbeError, WorkspaceProbe};

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

/// One tree's live child turns. The map and its lock are the hub's, because
/// the admission decision and the shutdown drain must be one lock; what the
/// map holds is the product's.
#[derive(Default)]
pub struct LiveChildTable {
    children: HashMap<String, Box<dyn LiveChildHandle>>,
    retiring: HashMap<String, Retirement>,
    starting: HashMap<String, ChildStartup>,
    retirement_recovery: Option<Arc<dyn Fn() + Send + Sync>>,
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
            .then(|| live.starting.get(agent_id).cloned())
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
                format!("{prefix} for {agent_id}: {error}; recover_agent required"),
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

/// The tree's one background recovery pass.
#[derive(Default)]
struct RecoveryPassSlot {
    task: Option<crate::task_set::TaskHandle<()>>,
    /// Set from start until the pass gives up the slot.
    running: bool,
    /// A start arrived while the pass ran.
    rerun: bool,
}

impl RecoveryPassSlot {
    /// A pass that panicked or was aborted no longer holds the slot.
    fn busy(&self) -> bool {
        self.running && self.task.as_ref().is_some_and(|task| !task.is_finished())
    }
}

/// What [`ProductTopologyRuntime::start_recovery_pass`] did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryPassRequest {
    /// A new pass started.
    Started,
    /// A pass was running; it runs once more before it finishes.
    Rerun,
    /// The hub is closed for shutdown.
    Closed,
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
    reattach: tokio::sync::Mutex<()>,
    /// Children whose fenced turn's owner is releasing and reopening their
    /// session outside the reattach lock. Recovery treats them as owned.
    recycling: Mutex<HashSet<String>>,
    /// The tree's background child-recovery pass, while one runs.
    recovery_pass: Mutex<RecoveryPassSlot>,
    /// Operator recovers of one child (`u`), separate from the background pass.
    /// Shutdown aborts every one before it releases leases.
    explicit_recovers: Mutex<Vec<crate::task_set::TaskHandle<()>>>,
    rejoin_spec: Mutex<Option<Arc<dyn Any + Send + Sync>>>,
    observer: Option<Arc<dyn TopologyObserver>>,
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

struct HubEntry {
    runtime: Arc<ProductTopologyRuntime>,
    root_leases: u32,
}

/// The process's attached trees: one working copy and live-child map per
/// tree, keyed by tree id. Owned by whoever attaches roots — nothing here is
/// a static — and looked up by every child compile, mail reopen, and rebind.
/// Hub lifetime per tree is root attach → last root lease release.
///
/// The hub mutex is held only to read or edit the map; no tree mutex is ever
/// taken under it, so a tree-holding caller may look the hub up.
#[derive(Default)]
pub struct TopologyHub {
    entries: Mutex<HashMap<String, HubEntry>>,
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
                            root_leases: 0,
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

    /// Takes one root lease on an attached tree, so the hub outlives every
    /// root that is still using it.
    ///
    /// # Errors
    ///
    /// Refuses when no runtime is attached under this id.
    pub fn acquire_root_lease(&self, tree_id: &str) -> TreeRuntimeResult<()> {
        let mut entries = self.entries()?;
        let entry = entries.get_mut(tree_id).ok_or_else(|| {
            TreeRuntimeError::new("topology hub is not attached; cannot acquire a root lease")
        })?;
        entry.root_leases = entry.root_leases.saturating_add(1);
        Ok(())
    }

    /// Looks the tree up, or load/bootstraps it only for an attaching root.
    /// `store` is asked for only on that bootstrap path: a hub hit opens
    /// nothing.
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
        } else if !is_root {
            return Err(TreeRuntimeError::new(
                "topology hub is not attached; child catalog compile cannot load or bootstrap a tree",
            ));
        } else if attach != TopologyHubAttach::AttachRoot {
            return Err(TreeRuntimeError::new(
                "topology hub is not attached; root catalog compile cannot bootstrap outside session attach",
            ));
        } else {
            let store = store()?;
            let tree = load_or_bootstrap(Arc::clone(&store)).await?;
            let vcs = tree.vcs_port();
            self.insert_root(tree_id, tree, store, vcs, observer)
                .await?
        };
        if attach == TopologyHubAttach::AttachRoot {
            self.acquire_root_lease(tree_id)?;
        }
        Ok(runtime)
    }

    /// Drops a root session lease; the last lease hands the runtime back
    /// for shutdown.
    fn release_root_lease(
        &self,
        tree_id: &str,
    ) -> TreeRuntimeResult<Option<Arc<ProductTopologyRuntime>>> {
        let mut entries = self.entries()?;
        let Some(entry) = entries.get_mut(tree_id) else {
            return Ok(None);
        };
        if entry.root_leases == 0 {
            return Ok(None);
        }
        entry.root_leases -= 1;
        if entry.root_leases > 0 {
            return Ok(None);
        }
        Ok(entries.remove(tree_id).map(|entry| entry.runtime))
    }

    /// Removes a tree regardless of leases (a failed root attach, a test
    /// teardown); `unleased_only` keeps a leased tree in place.
    #[cfg(any(test, feature = "test-support"))]
    fn evict(&self, tree_id: &str, unleased_only: bool) -> Option<Arc<ProductTopologyRuntime>> {
        let mut entries = self.entries.lock().ok()?;
        match entries.get(tree_id) {
            Some(entry) if !unleased_only || entry.root_leases == 0 => {
                entries.remove(tree_id).map(|entry| entry.runtime)
            }
            _ => None,
        }
    }
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
    root: &Path,
) -> Result<Option<Arc<AtomicU64>>, ProbeError> {
    let probe: &dyn WorkspaceProbe = &LocalWorkspaceProbe;
    let root = resolved_or_recorded(probe, root)?;
    for runtime in process_hub().runtimes() {
        let Ok(rows) = runtime.driver().read(ProductAgentTree::workspace_paths) else {
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
pub fn workspace_base_for_root(root: &Path) -> Result<Option<String>, ProbeError> {
    let probe: &dyn WorkspaceProbe = &LocalWorkspaceProbe;
    let root = resolved_or_recorded(probe, root)?;
    for runtime in process_hub().runtimes() {
        let Ok(rows) = runtime.driver().read(|tree| {
            tree.workspaces()
                .values()
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
        Ok(())
    }
}
impl std::error::Error for TopologyShutdownError {}

/// Whether this tree reopened paused, treating "no hub yet" as not paused.
///
/// Callers that must not race the attach ask the `Option`-returning form
/// instead; this one is for the readers that only render a state.
#[must_use]
pub fn is_restored_paused(tree_id: &str) -> bool {
    hub_tree_is_restored_paused(tree_id).unwrap_or(false)
}

/// `None` while the hub is not registered yet. `--resume-tree` must Wait on
/// that gap instead of treating a missing hub as Live.
///
/// Reads the driver's atomic mirror of the last installed tree state, so the
/// JSONL thread cannot deadlock against ResumeTree's tree mutex. Contention
/// never manufactures a paused state.
pub fn hub_tree_is_restored_paused(tree_id: &str) -> Option<bool> {
    let runtime = lookup(tree_id)?;
    Some(runtime.driver.is_restored_paused())
}

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
