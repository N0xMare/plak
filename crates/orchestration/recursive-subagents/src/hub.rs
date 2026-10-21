//! Process-local topology hub: one working copy and live-child map per tree.
//!
//! SQLite remains durable authority. The hub is the in-memory working copy for
//! one root session attach. Catalog compile looks the hub up; it must not
//! `load_tree` on child or mail-reopen catalogs (that would pause the tree and
//! mint a new `Arc`). Hub lifetime is root attach → last root lease release.

mod startup;
pub use startup::{ChildStartup, ChildStartupStage, STARTUP_SLOT_OCCUPIED};

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::FutureExt;

use plakous::agent::{Agent, CommandId, TurnControl, TurnResultHandle};
use plakous::model::TurnId;

use crate::tools::TreeDriver;
use crate::tree::{ProductAgentTree, TopologyObserver, TreeStore};
use crate::tree::{TreeRuntimeError, TreeRuntimeResult};
use plakous_workspace::occupancy_vcs::VcsOps;

/// Whether this catalog compile may create the process hub for a missing root.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TopologyHubAttach {
    /// Child spawn, mail reopen, compact, and rebind: lookup only.
    LookupOnly,
    /// Root `build_or_open` may load-or-bootstrap and insert the hub once.
    AttachRoot,
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
    fn cancel_and_release(
        self: Box<Self>,
        driver: TreeDriver,
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

impl LiveChildTable {
    /// An active child; retiring children cannot accept new work.
    pub fn get(&self, agent_id: &str) -> Option<&(dyn LiveChildHandle + 'static)> {
        self.children.get(agent_id).map(Box::as_ref)
    }
    /// The active child under its admission lock.
    pub fn get_mut(&mut self, agent_id: &str) -> Option<&mut (dyn LiveChildHandle + 'static)> {
        self.children.get_mut(agent_id).map(Box::as_mut)
    }
    /// Whether an active or retiring child owns this agent slot.
    pub fn contains_key(&self, agent_id: &str) -> bool {
        self.children.contains_key(agent_id)
            || self.retiring.contains_key(agent_id)
            || self.starting.contains_key(agent_id)
    }
    /// All active and retiring agent slots.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.children
            .keys()
            .chain(self.retiring.keys())
            .chain(self.starting.keys())
    }
    /// Whether every active and retiring slot is vacant.
    pub fn is_empty(&self) -> bool {
        self.children.is_empty() && self.retiring.is_empty() && self.starting.is_empty()
    }
    fn start_retire(
        &mut self,
        live: Arc<tokio::sync::Mutex<Self>>,
        driver: TreeDriver,
        agent_id: &str,
        expected: Option<TurnId>,
        cancel: bool,
    ) -> Option<Retirement> {
        if let Some(retirement) = self.retiring.get(agent_id) {
            return expected
                .is_none_or(|turn| turn == retirement.turn)
                .then(|| retirement.clone());
        }
        let turn = self.children.get(agent_id)?.turn_id();
        if expected.is_some_and(|expected| expected != turn) {
            return None;
        }
        let child = self.children.remove(agent_id)?;
        let agent = child.agent();
        let agent_id = agent_id.to_owned();
        let worker_id = agent_id.clone();
        let expected_attempt = driver
            .read(|tree| {
                tree.attempt(&worker_id)
                    .map(|attempt| attempt.attempt_id.clone())
            })
            .ok()
            .flatten();
        let retirement_attempt = expected_attempt.clone();
        let recovery = self.retirement_recovery.clone();
        // Spawn before yielding the admission lock: no caller cancellation can
        // strand an extracted child between reservation and task ownership.
        let task = tokio::spawn(async move {
            let started = tokio::time::Instant::now();
            let mut release_returned = false;
            let mut cleanup_failure = None;
            let mut fault_recorded = false;
            let release_driver = driver.clone();
            let release = async move {
                if cancel {
                    child.cancel_and_release(release_driver).await
                } else {
                    child.release(release_driver).await
                }
            };
            let result = std::panic::AssertUnwindSafe(async {
                tokio::pin!(release);
                let released = tokio::select! {
                    released = &mut release => released,
                    () = tokio::time::sleep(HOST_RETIRE_JOIN_TIMEOUT) => {
                        tracing::warn!(target: "plak.topology", agent_id = %worker_id,
                            "slow child retirement; release owner retained");
                        record_retirement_fault(&driver, &worker_id, expected_attempt.as_ref(), RetirementFaultSource::Owner, RETIRE_JOIN_EXPIRED);
                        fault_recorded = true;
                        release.await
                    }
                };
                release_returned = true;
                cleanup_failure.clone_from(&released.failure);
                publish_release(&driver, &worker_id, released)
            })
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err("child retirement worker panicked".to_owned()));
            if let Err(error) = &result {
                record_retirement_fault(
                    &driver,
                    &worker_id,
                    expected_attempt.as_ref(),
                    RetirementFaultSource::Owner,
                    error,
                );
                fault_recorded = true;
            }
            {
                let mut live = live.lock().await;
                if live
                    .retiring
                    .get(&worker_id)
                    .is_some_and(|retirement| retirement.turn == turn)
                {
                    live.retiring.remove(&worker_id);
                    let _ = driver.mutate(|tree| tree.release_turn_owner(&worker_id));
                }
            }
            tracing::info!(target: "plak.topology", agent_id = %worker_id,
                elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                release_returned, cleanup_failure = ?cleanup_failure,
                success = result.is_ok(), retirement_failure = ?result.as_ref().err(),
                "child retirement returned");
            if (fault_recorded || retirement_needs_recovery(&driver, &worker_id))
                && let Some(recovery) = recovery
            {
                recovery();
            }
            result
        });
        let retirement = Retirement {
            agent,
            turn,
            expected_attempt: retirement_attempt,
            abort: task.abort_handle(),
            completion: task
                .map(|result| {
                    result.unwrap_or_else(|error| {
                        Err(if error.is_panic() {
                            "child retirement worker panicked".to_owned()
                        } else {
                            "child retirement worker was aborted".to_owned()
                        })
                    })
                })
                .boxed()
                .shared(),
        };
        self.retiring.insert(agent_id, retirement.clone());
        Some(retirement)
    }
    #[cfg(any(test, feature = "test-support"))]
    /// Installs a test child without runtime admission.
    pub fn insert_for_test(&mut self, agent_id: String, child: Box<dyn LiveChildHandle>) {
        self.children.insert(agent_id, child);
    }
}

#[derive(Clone)]
struct Retirement {
    agent: Option<Agent>,
    turn: TurnId,
    expected_attempt: Option<crate::protocol::AttemptId>,
    completion:
        futures_util::future::Shared<futures_util::future::BoxFuture<'static, Result<(), String>>>,
    abort: tokio::task::AbortHandle,
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
        return startup.join_cancel().await;
    }
    let retirement =
        live.lock()
            .await
            .start_retire(live.clone(), driver.clone(), agent_id, expected, cancel);
    match retirement {
        Some(retirement) => {
            if let Ok(result) =
                tokio::time::timeout(HOST_RETIRE_JOIN_TIMEOUT, retirement.completion).await
            {
                result
                    .map(|()| LiveCancelOutcome::Cancelled)
                    .map_err(LiveCancelError::OwnerFailure)
            } else {
                let recovery = live.lock().await.retirement_recovery.clone();
                if let Some(recovery) = recovery {
                    recovery();
                }
                Err(LiveCancelError::JoinExpired)
            }
        }
        None => Ok(LiveCancelOutcome::NotLive),
    }
}

fn publish_release(
    driver: &TreeDriver,
    agent_id: &str,
    release: LiveRelease,
) -> Result<(), String> {
    let publication = (|| {
        let (result, effects) = driver
            .mutate(|tree| {
                if tree.revision != tree.stored_revision {
                    driver.save(tree).map_err(|error| error.message)?;
                }
                driver
                    .fold_and_commit(tree, |tree| {
                        if let Some(feed) = release.feed {
                            feed.apply(tree, agent_id)?;
                        }
                        Ok(())
                    })
                    .map_err(|error| error.message)
            })
            .map_err(|error| error.message)?;
        result?;
        driver
            .settle(effects)
            .map(|_| ())
            .map_err(|error| error.message)
    })();
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
    Joiner,
}

fn record_retirement_fault(
    driver: &TreeDriver,
    agent_id: &str,
    expected: Option<&crate::protocol::AttemptId>,
    source: RetirementFaultSource,
    error: &str,
) {
    tracing::error!(target: "plak.topology", agent_id, error, "child retirement failed");
    // Fault reporting must not unwind out of the release it is fencing.
    let recorded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        driver.mutate_and_settle(|tree| {
            // Only the live release owner can fence a committed terminal result:
            // terminal observation itself does not establish successful release.
            // A different current attempt can only be an unstarted followup:
            // the retiring slot still excludes startup. Fence it rather than
            // allowing rollback to erase the owner's release failure.
            let fenced = match source {
                RetirementFaultSource::Owner => tree
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
        })
    }));
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
    task: Option<tokio::task::JoinHandle<()>>,
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
    /// The tree, under the one mutex every mutation takes.
    ///
    /// `pub(crate)` and not `pub`, for the reason [`TreeDriver`]'s handle is:
    /// a caller above this crate that locked it could mutate and drop the
    /// guard without taking the [`crate::tree::Effects`] the mutation queued.
    /// Outside callers get [`Self::read`], [`Self::try_read`] and
    /// [`Self::mutate_and_settle`].
    pub(crate) tree: Arc<Mutex<ProductAgentTree>>,
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
    rejoin_spec: Mutex<Option<Arc<dyn Any + Send + Sync>>>,
    observer: Option<Arc<dyn TopologyObserver>>,
}

impl ProductTopologyRuntime {
    /// Installs the host's recovery trigger for returned startup/retirement owners.
    /// The callback must not retain this runtime; shutdown closes pass admission.
    pub async fn set_retirement_recovery(&self, recovery: Arc<dyn Fn() + Send + Sync>) {
        self.driver.set_retirement_recovery(recovery.clone());
        self.live.lock().await.retirement_recovery = Some(recovery);
    }
    /// Takes ownership of `tree`, binding it to `store` (one connection for
    /// the runtime's life) and `vcs` (installed as the tree's port).
    pub fn new(
        tree: ProductAgentTree,
        store: Arc<dyn TreeStore>,
        vcs: Arc<dyn VcsOps>,
        observer: Option<Arc<dyn TopologyObserver>>,
    ) -> Self {
        let tree = Arc::new(Mutex::new(tree));
        let driver = TreeDriver::new(Arc::clone(&tree), store, vcs, observer.clone());
        Self {
            tree,
            live: Arc::new(tokio::sync::Mutex::new(LiveChildTable::default())),
            driver,
            closed: AtomicBool::new(false),
            reattach: tokio::sync::Mutex::new(()),
            recycling: Mutex::new(HashSet::new()),
            recovery_pass: Mutex::new(RecoveryPassSlot::default()),
            rejoin_spec: Mutex::new(None),
            observer,
        }
    }

    /// The tree with its store and VCS port. Every mutation, effect and
    /// publish for this tree goes through it.
    pub fn driver(&self) -> &TreeDriver {
        &self.driver
    }

    /// Raw tree access for synthetic fixtures; absent from production builds.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn tree_handle_for_test(&self) -> &Arc<Mutex<ProductAgentTree>> {
        &self.tree
    }

    /// Reads this tree under its mutex.
    ///
    /// # Errors
    ///
    /// Refuses when the tree mutex is poisoned.
    pub fn read<R>(
        &self,
        work: impl FnOnce(&ProductAgentTree) -> R,
    ) -> Result<R, crate::tree::TopologyError> {
        self.driver.read(work)
    }

    /// Reads this tree without waiting for its mutex; `None` when it is held.
    pub fn try_read<R>(&self, work: impl FnOnce(&ProductAgentTree) -> R) -> Option<R> {
        self.driver.try_read(work)
    }

    /// One mutation on this tree, with everything it queued settled.
    ///
    /// # Errors
    ///
    /// Refuses a poisoned mutex and reports a failed settle.
    pub fn mutate_and_settle<R>(
        &self,
        work: impl FnOnce(&mut ProductAgentTree) -> R,
    ) -> Result<(R, crate::tree::Settled), crate::tree::TopologyError> {
        self.driver.mutate_and_settle(work)
    }

    /// One mutation, published, and everything it queued settled — the whole
    /// cycle in one call, so none of the three halves can be forgotten.
    ///
    /// This is what a caller above this crate reaches for: `work` gets the
    /// tree under its mutex, the tree is published while still locked, and
    /// the effects the mutation queued run with the lock released.
    ///
    /// # Errors
    ///
    /// Refuses a poisoned mutex, a refused publish, and a failed settle;
    /// `work`'s own refusal comes back unchanged. Refused consequences are
    /// rolled back and no unacknowledged effect runs.
    pub fn mutate_then_emit<R>(
        &self,
        epoch_advanced: bool,
        work: impl FnOnce(&mut ProductAgentTree) -> Result<R, String>,
    ) -> Result<R, String> {
        let (result, effects) = {
            let mut tree = self
                .tree
                .lock()
                .map_err(|_| "topology tree lock is poisoned".to_owned())?;
            if tree.revision != tree.stored_revision {
                self.driver.save(&mut tree).map_err(|error| error.message)?;
            }
            let result = self
                .driver
                .fold_and_commit(&mut tree, |tree| {
                    work(tree).map_err(|message| {
                        crate::tree::TopologyError::new(
                            crate::protocol::TopologyErrorCode::OperationRecoveryRequired,
                            message,
                        )
                    })
                })
                .map_err(|error| error.message)?;
            if epoch_advanced && let Some(observer) = self.observer.as_ref() {
                observer.epoch_advanced(&tree.tree_id);
            }
            (result, tree.take_effects())
        };
        let settled = self.driver.settle(effects).map_err(|error| error.message);
        settled?;
        Ok(result)
    }

    /// Installs process-local ownership before the consequence snapshot.
    /// `prepare` must change no durable rows or reserve external effects.
    /// Its registrations remain live on a refused save until their owners drop.
    ///
    /// # Errors
    /// Reports failed preparation, publication, or effect settlement.
    pub fn prepare_then_emit<R>(
        &self,
        prepare: impl FnOnce(&mut ProductAgentTree) -> Result<R, String>,
    ) -> Result<R, String> {
        let (result, effects) = {
            let mut tree = self.driver.lock().map_err(|error| error.message)?;
            if tree.revision != tree.stored_revision {
                self.driver.save(&mut tree).map_err(|error| error.message)?;
            }
            let result = prepare(&mut tree)?;
            self.driver
                .fold_and_commit(&mut tree, |_| Ok(()))
                .map_err(|error| error.message)?;
            (result, tree.take_effects())
        };
        self.driver.settle(effects).map_err(|error| error.message)?;
        Ok(result)
    }

    /// Records and publishes an intent before folding its consequences.
    /// Process-local ownership changes in `record` are outside the revert
    /// boundary. The callback must not publish the tree itself.
    ///
    /// # Errors
    /// Reports a failed recording, either publish, or effect settlement.
    pub fn record_then_apply<R>(
        &self,
        agent_id: &str,
        record: impl FnOnce(&mut ProductAgentTree) -> Result<R, crate::tree::TopologyError>,
    ) -> Result<R, String> {
        let (result, effects) = {
            let mut tree = self.driver.lock().map_err(|error| error.message)?;
            let result = record(&mut tree).map_err(|error| error.message)?;
            self.driver
                .apply_recorded(&mut tree, agent_id)
                .map_err(|error| error.message)?;
            (result, tree.take_effects())
        };
        self.driver.settle(effects).map_err(|error| error.message)?;
        Ok(result)
    }

    /// Fenced whole-tree publish of the runtime's tree; `epoch_advanced`
    /// reports the install to the observer once the rows are durable, never
    /// before, so nothing is announced that a refused publish did not keep.
    pub fn save_then_emit(
        &self,
        tree: &mut ProductAgentTree,
        epoch_advanced: bool,
    ) -> Result<(), String> {
        self.driver.save(tree).map_err(|error| error.message)?;
        if epoch_advanced && let Some(observer) = self.observer.as_ref() {
            observer.epoch_advanced(&tree.tree_id);
        }
        Ok(())
    }

    /// This tree's telemetry port, when one was installed.
    pub fn observer(&self) -> Option<Arc<dyn TopologyObserver>> {
        self.observer.as_ref().map(Arc::clone)
    }

    /// Parks the host's rejoin spec on the hub for this tree's lifetime.
    ///
    /// Opaque on purpose: what a spawn's parent *is* — its session, its
    /// account, its composition — is entirely the host's, and the hub only
    /// needs somewhere to keep it that dies when the hub does. Storing it as
    /// `Any` is what makes that literally true: the tree cannot read it even
    /// by accident, and [`Self::rejoin_spec`] hands it back to the one caller
    /// that knows its type.
    pub fn remember_rejoin_spec(&self, parent: Arc<dyn Any + Send + Sync>) {
        if let Ok(mut slot) = self.rejoin_spec.lock() {
            *slot = Some(parent);
        }
    }

    /// The parked rejoin spec, for the host to downcast to its own type.
    pub fn rejoin_spec(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.rejoin_spec
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(Arc::clone))
    }

    /// Whether this hub has been closed for shutdown. A closed hub admits no
    /// live turn, which is what makes the drain final.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Serializes crash-cut reattach so overlapping root attaches wait for
    /// `resume_tree` instead of exposing a still-paused tree.
    pub async fn lock_reattach(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.reattach.lock().await
    }

    /// Marks `agent_id` as owned by its turn waiter's recycle, which runs
    /// without the reattach lock. Set under that lock before the live entry
    /// goes, so a recovery never sees the child unowned mid-recycle. The mark
    /// lasts until the returned guard drops, so a recycle that panics or is
    /// cancelled cannot leave the child owned for good.
    #[must_use = "the mark is cleared when the guard drops"]
    pub fn mark_recycling(self: &Arc<Self>, agent_id: &str) -> RecyclingGuard {
        if let Ok(mut recycling) = self.recycling.lock() {
            recycling.insert(agent_id.to_owned());
        }
        RecyclingGuard {
            runtime: Arc::clone(self),
            agent_id: agent_id.to_owned(),
        }
    }

    /// Whether a turn waiter is recycling `agent_id` right now.
    pub fn is_recycling(&self, agent_id: &str) -> bool {
        self.recycling
            .lock()
            .is_ok_and(|recycling| recycling.contains(agent_id))
    }

    /// Starts this tree's background recovery pass with `spawn`. While one is
    /// running, the request is not dropped: the running pass runs once more
    /// before it finishes (see [`Self::finish_recovery_pass`]). A closed hub
    /// starts nothing.
    pub fn start_recovery_pass(
        &self,
        spawn: impl FnOnce() -> tokio::task::JoinHandle<()>,
    ) -> RecoveryPassRequest {
        let Ok(mut slot) = self.recovery_pass.lock() else {
            return RecoveryPassRequest::Closed;
        };
        if self.is_closed() {
            return RecoveryPassRequest::Closed;
        }
        if slot.busy() {
            slot.rerun = true;
            return RecoveryPassRequest::Rerun;
        }
        slot.rerun = false;
        slot.running = true;
        slot.task = Some(spawn());
        RecoveryPassRequest::Started
    }

    /// Called by a pass when it is done: `true` when a request arrived while
    /// it ran and it must run once more; otherwise the slot is released.
    /// Decided under the slot lock, so no request can fall between the
    /// answer and the pass ending.
    pub fn finish_recovery_pass(&self) -> bool {
        let Ok(mut slot) = self.recovery_pass.lock() else {
            return false;
        };
        if slot.rerun && !self.is_closed() {
            slot.rerun = false;
            return true;
        }
        slot.running = false;
        false
    }

    /// Whether this tree's background recovery pass is still running.
    pub fn recovery_pass_running(&self) -> bool {
        self.recovery_pass.lock().is_ok_and(|slot| slot.busy())
    }

    /// The reattach lock without waiting, so a test can prove a second
    /// reattach is actually excluded rather than merely slow.
    #[cfg(any(test, feature = "test-support"))]
    pub fn try_lock_reattach(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, ()>, tokio::sync::TryLockError> {
        self.reattach.try_lock()
    }

    /// Stores `child` if this agent is vacant and the hub is still open.
    /// Returns `Err(child)` when the caller must `release_opened` the loser.
    /// The one admission decision for a live turn, taken with the live map
    /// held. A closed tree admits nothing, and one agent holds
    /// at most one turn.
    ///
    /// This is the invariant the live table must keep when it moves to the
    /// product: the closed flag is read *under the same lock the insert
    /// takes*, and `shutdown_runtime` sets that flag before it takes the lock
    /// to drain. So a turn either wins the lock before the drain — and is in
    /// the set the drain takes — or loses it and reads the flag the drain
    /// already set, and is refused. There is no third outcome, and no turn is
    /// left running with nothing holding it.
    fn admits_live(&self, live: &LiveChildTable, agent_id: &str) -> bool {
        !self.is_closed() && !live.contains_key(agent_id)
    }

    /// Takes the live slot for one agent, or hands `child` back when the slot
    /// is taken or the hub is closed.
    ///
    /// The check and the insert happen under one hold of the live lock, so
    /// two turns racing for one agent cannot both win; the loser is returned
    /// rather than dropped, because only its owner can release it.
    ///
    /// # Errors
    ///
    /// Returns the rejected handle when the agent is occupied or the hub is
    /// closed.
    pub async fn try_occupy_live(
        &self,
        agent_id: String,
        child: Box<dyn LiveChildHandle>,
    ) -> Result<TurnId, Box<dyn LiveChildHandle>> {
        self.try_occupy_live_admitted(agent_id, child, || Ok(()))
            .await
            .map_err(|(child, _)| child)
    }

    /// [`Self::try_occupy_live`], admitted only if `admit` (which may
    /// publish the turn's binding) accepts under the live lock. An
    /// interrupt's cancel needs that lock, so it either lands before `admit`
    /// runs or finds and cancels the occupied turn.
    ///
    /// # Errors
    ///
    /// Returns the rejected handle, with `admit`'s refusal when it refused.
    pub async fn try_occupy_live_admitted(
        &self,
        agent_id: String,
        child: Box<dyn LiveChildHandle>,
        admit: impl FnOnce() -> Result<(), String>,
    ) -> Result<TurnId, (Box<dyn LiveChildHandle>, Option<String>)> {
        if self.is_closed() {
            return Err((child, None));
        }
        let mut live = self.live.lock().await;
        if !self.admits_live(&live, &agent_id) {
            return Err((child, None));
        }
        if let Err(refusal) = admit() {
            return Err((child, Some(refusal)));
        }
        let turn_id = child.turn_id();
        if let Err(error) = self
            .driver
            .mutate(|tree| tree.bind_turn_owner(&agent_id, turn_id.to_string()))
        {
            return Err((child, Some(error.message)));
        }
        live.children.insert(agent_id, child);
        Ok(turn_id)
    }

    /// Marks the tree closed: the flag every later admission reads. Set
    /// before the drain takes the live lock, never after.
    fn close_for_shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    /// Cancels or joins the retirement already owning this agent slot.
    pub async fn cancel_live(&self, agent_id: &str) -> Result<LiveCancelOutcome, LiveCancelError> {
        retire_live(&self.driver, &self.live, agent_id, None, true).await
    }

    /// Cancels live children concurrently, reporting in input order (close
    /// supplies postorder). Each exact retirement keeps its own bound.
    ///
    /// Every agent lands in exactly one bucket: cancelled, not live,
    /// owner-confirmed failure, or joiner expiry. The close path needs
    /// not-live separated from cancelled —
    /// "there was nothing running" and "the turn was stopped" are different
    /// answers, and folding them into one `Ok(())` hid the difference.
    pub async fn cancel_live_many(&self, agent_ids: &[String]) -> LiveCancelReport {
        let mut report = LiveCancelReport::default();
        let outcomes =
            futures_util::future::join_all(agent_ids.iter().map(|agent_id| async move {
                let expected_attempt_id = self
                    .driver
                    .read(|tree| {
                        tree.attempt(agent_id)
                            .map(|attempt| attempt.attempt_id.clone())
                    })
                    .ok()
                    .flatten();
                (expected_attempt_id, self.cancel_live(agent_id).await)
            }))
            .await;
        for (agent_id, (expected_attempt_id, outcome)) in agent_ids.iter().zip(outcomes) {
            match outcome {
                Ok(LiveCancelOutcome::Cancelled) => report.cancelled.push(agent_id.clone()),
                Ok(LiveCancelOutcome::NotLive) => report.not_live.push(agent_id.clone()),
                Err(LiveCancelError::JoinExpired) => {
                    report.joiner_expired.push(agent_id.clone());
                    report
                        .first_failure
                        .get_or_insert_with(|| RETIRE_JOIN_EXPIRED.to_owned());
                }
                Err(LiveCancelError::OwnerFailure(message)) => {
                    report.failed.push(agent_id.clone());
                    report.owner_failures.push(LiveRetirementFailure {
                        agent_id: agent_id.clone(),
                        expected_attempt_id,
                        message: message.clone(),
                    });
                    if report.first_failure.is_none() {
                        report.first_failure = Some(message);
                    }
                }
            }
        }
        report
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
    /// Agent whose release failed.
    pub agent_id: String,
    /// Attempt owning the release, or none for an unattached root.
    pub expected_attempt_id: Option<crate::protocol::AttemptId>,
    /// The owner's cleanup failure.
    pub message: String,
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
    pub fn insert_root(
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
        let effects = runtime
            .driver
            .fold_startup()
            .map_err(|error| TreeRuntimeError::new(error.message));
        let mut entries = self.entries()?;
        if let Some(entry) = entries.get(&tree_id) {
            let occupant = Arc::clone(&entry.runtime);
            drop(entries);
            if let Ok(effects) = effects {
                runtime
                    .driver
                    .settle(effects)
                    .map_err(|error| TreeRuntimeError::new(error.message))?;
            }
            return Ok(occupant);
        }
        let effects = effects?;
        entries.insert(
            tree_id,
            HubEntry {
                runtime: Arc::clone(&runtime),
                root_leases: 0,
            },
        );
        drop(entries);
        runtime
            .driver
            .settle(effects)
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
    pub fn resolve(
        &self,
        tree_id: &str,
        is_root: bool,
        attach: TopologyHubAttach,
        store: impl FnOnce() -> TreeRuntimeResult<Arc<dyn TreeStore>>,
        observer: Option<Arc<dyn TopologyObserver>>,
        load_or_bootstrap: impl FnOnce(&dyn TreeStore) -> TreeRuntimeResult<ProductAgentTree>,
    ) -> TreeRuntimeResult<Arc<ProductTopologyRuntime>> {
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
            let tree = load_or_bootstrap(store.as_ref())?;
            let vcs = tree.vcs_port();
            self.insert_root(tree_id, tree, store, vcs, observer)?
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

/// Observational Read/Grep stamp for a session workspace root, if a hub tree owns it.
pub fn generation_stamp_for_workspace_root(root: &Path) -> Option<Arc<AtomicU64>> {
    for runtime in process_hub().runtimes() {
        let Ok(tree) = runtime.tree.lock() else {
            continue;
        };
        if let Some(stamp) = tree.generation_counter_for_root(root) {
            return Some(stamp);
        }
    }
    None
}

/// Inserts a working copy over an in-memory store, or returns the occupant
/// if this tree is already attached. Test attach only: production roots
/// attach through [`resolve_topology_runtime`] with the product's store.
#[cfg(any(test, feature = "test-support"))]
pub fn insert_root(
    tree_id: impl Into<String>,
    tree: ProductAgentTree,
    observer: Option<Arc<dyn TopologyObserver>>,
) -> TreeRuntimeResult<Arc<ProductTopologyRuntime>> {
    let vcs = tree.vcs_port();
    process_hub().insert_root(
        tree_id,
        tree,
        Arc::new(crate::tree::MemoryTreeStore::new()),
        vcs,
        observer,
    )
}

/// Inserts a working copy over an explicit store, or returns the occupant.
#[cfg(any(test, feature = "test-support"))]
pub fn insert_root_with(
    tree_id: impl Into<String>,
    tree: ProductAgentTree,
    store: Arc<dyn TreeStore>,
    observer: Option<Arc<dyn TopologyObserver>>,
) -> TreeRuntimeResult<Arc<ProductTopologyRuntime>> {
    let vcs = tree.vcs_port();
    process_hub().insert_root(tree_id, tree, store, vcs, observer)
}

/// Looks up the hub, or load/bootstraps it only for an attaching root, over
/// the store the product hands out for it (asked for on that path alone).
pub fn resolve_topology_runtime(
    tree_id: &str,
    is_root: bool,
    attach: TopologyHubAttach,
    store: impl FnOnce() -> TreeRuntimeResult<Arc<dyn TreeStore>>,
    observer: Option<Arc<dyn TopologyObserver>>,
    load_or_bootstrap: impl FnOnce(&dyn TreeStore) -> TreeRuntimeResult<ProductAgentTree>,
) -> TreeRuntimeResult<Arc<ProductTopologyRuntime>> {
    process_hub().resolve(tree_id, is_root, attach, store, observer, load_or_bootstrap)
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

/// Drops a root session lease. The last lease drains live children and removes the hub.
pub async fn release_root_lease(tree_id: &str) -> Result<(), TopologyShutdownError> {
    let runtime =
        process_hub()
            .release_root_lease(tree_id)
            .map_err(|error| TopologyShutdownError {
                tree_failure: Some(error.to_string()),
                child_failures: Vec::new(),
            })?;
    if let Some(runtime) = runtime {
        shutdown_runtime(runtime).await?;
    }
    Ok(())
}

/// Closes admission and aborts recovery before collecting runtime capabilities.
/// Unlike hub retirement, this neither settles providers nor saves tree rows.
pub async fn abandon_live_agents(tree_id: &str) -> Vec<Agent> {
    let Some(runtime) = lookup(tree_id) else {
        return Vec::new();
    };
    runtime.close_for_shutdown();
    let pass = runtime.recovery_pass.lock().ok().and_then(|mut slot| {
        slot.running = false;
        slot.rerun = false;
        slot.task.take()
    });
    if let Some(pass) = pass {
        pass.abort();
        let _ = pass.await;
    }
    let live = runtime.live.lock().await;
    for startup in live.starting.values() {
        startup.request_cancel();
    }
    for retirement in live.retiring.values() {
        retirement.abort.abort();
    }
    live.children
        .values()
        .filter_map(|child| child.agent())
        .chain(
            live.retiring
                .values()
                .filter_map(|retirement| retirement.agent.clone()),
        )
        .collect()
}

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
/// Uses `try_lock` so the JSONL thread cannot deadlock against ResumeTree's
/// tree mutex.
pub fn hub_tree_is_restored_paused(tree_id: &str) -> Option<bool> {
    let runtime = lookup(tree_id)?;
    Some(match runtime.tree.try_lock() {
        Ok(tree) => tree.state == crate::tree::TreeAdmissionState::RestoredPaused,
        Err(std::sync::TryLockError::WouldBlock) => true,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => {
            poisoned.into_inner().state == crate::tree::TreeAdmissionState::RestoredPaused
        }
    })
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

/// Shutdown joins children concurrently within this allowance. Expiry is a
/// bounded refusal, not cancellation of the workers that still own release.
const HOST_SHUTDOWN_RELEASE_TIMEOUT: Duration = Duration::from_secs(2);
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

async fn startup_results_owned(startups: Vec<(String, ChildStartup)>) -> Vec<Result<(), String>> {
    futures_util::future::join_all(startups.iter().map(|(_, startup)| async move {
        startup
            .join_cancel()
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }))
    .await
}

async fn shutdown_runtime(
    runtime: Arc<ProductTopologyRuntime>,
) -> Result<(), TopologyShutdownError> {
    runtime.close_for_shutdown();
    // A recovery pass reopens child sessions; it ends (or is aborted) before
    // the drain, so no reopen outlives the leases this shutdown releases.
    let pass = runtime
        .recovery_pass
        .lock()
        .ok()
        .and_then(|mut slot| slot.task.take());
    if let Some(mut pass) = pass
        && tokio::time::timeout(HOST_SHUTDOWN_RELEASE_TIMEOUT, &mut pass)
            .await
            .is_err()
    {
        pass.abort();
        let _ = tokio::time::timeout(HOST_SHUTDOWN_DETACH_TIMEOUT, pass).await;
    }
    let mut failure = TopologyShutdownError::default();
    if let Ok(mut tree) = runtime.tree.lock() {
        tree.emit_shutdown_detaches(runtime.observer().as_ref());
        // Detaches are observations, not rows: nothing to publish unless the
        // tree moved since the store last acknowledged it.
        if tree.revision != tree.stored_revision
            && let Err(error) = runtime.driver.save(&mut tree)
        {
            tracing::warn!(
                target: "plak.topology",
                tree_id = %tree.tree_id,
                error = %error.message,
                "tree was not persisted at hub shutdown"
            );
            failure.tree_failure = Some(error.message);
        }
    } else {
        failure.tree_failure = Some("topology tree lock is poisoned".into());
    }
    let startups = {
        let live = runtime.live.lock().await;
        live.starting
            .iter()
            .map(|(id, startup)| {
                startup.request_cancel();
                (id.clone(), startup.clone())
            })
            .collect::<Vec<_>>()
    };
    let startup_task = tokio::spawn(startup_results_owned(startups.clone()));
    let startup_abort = startup_task.abort_handle();
    let retirements = {
        let mut live = runtime.live.lock().await;
        let ids: Vec<_> = live.children.keys().cloned().collect();
        for id in ids {
            live.start_retire(
                runtime.live.clone(),
                runtime.driver.clone(),
                &id,
                None,
                true,
            );
        }
        live.retiring
            .iter()
            .map(|(id, retirement)| (id.clone(), retirement.clone()))
            .collect::<Vec<_>>()
    };
    let completions = || {
        futures_util::future::join_all(
            retirements
                .iter()
                .map(|(_, retirement)| retirement.completion.clone()),
        )
    };
    // Shutdown bounds the join, not owned release. Only explicit process-exit
    // abandonment may abort a retirement while it still owns runtime authority.
    let results = match tokio::time::timeout(HOST_SHUTDOWN_RELEASE_TIMEOUT, completions()).await {
        Ok(results) => results,
        Err(_) => retirements
            .iter()
            .map(|_| Err("child retirement exceeded the shutdown bound; owner retained".to_owned()))
            .collect(),
    };
    for ((agent_id, retirement), result) in retirements.into_iter().zip(results) {
        if let Err(error) = result {
            record_retirement_fault(
                &runtime.driver,
                &agent_id,
                retirement.expected_attempt.as_ref(),
                RetirementFaultSource::Joiner,
                &error,
            );
            failure.child_failures.push((agent_id, error));
        }
    }
    let startup_results = match tokio::time::timeout(HOST_SHUTDOWN_DETACH_TIMEOUT, startup_task)
        .await
    {
        Ok(Ok(results)) => results,
        Ok(Err(_)) => {
            vec![Err("child startup settlement task failed".into()); startups.len()]
        }
        Err(_) => {
            startup_abort.abort();
            vec![Err("child startup settlement exceeded the shutdown bound".into()); startups.len()]
        }
    };
    for ((id, startup), result) in startups.into_iter().zip(startup_results) {
        if let Err(error) = result {
            record_retirement_fault(
                &runtime.driver,
                &id,
                startup.expected_attempt.as_ref(),
                RetirementFaultSource::Joiner,
                &error,
            );
            failure.child_failures.push((id, error));
        }
    }
    if failure.tree_failure.is_none() && failure.child_failures.is_empty() {
        Ok(())
    } else {
        Err(failure)
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
        let runtime = insert_root(tree_id.clone(), bootstrap(&tree_id, root), None).unwrap();
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
                |_| Err(TreeRuntimeError::new("a hub hit must not load a tree")),
            )
            .unwrap();
            assert!(Arc::ptr_eq(&runtime.tree, &resolved.tree));
        }
        assert!(!opened.load(Ordering::SeqCst));
        force_release(&tree_id).await;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests {
    use super::test_support;
    use super::test_support::{bootstrap, memory_store, unique_tree_id};
    use super::*;

    use crate::tree::ProductAgentTree;

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct UsageObserver(Mutex<Vec<crate::tree::ProjectedChildTurnUsage>>);

    impl TopologyObserver for UsageObserver {
        fn turn_usage(&self, terminal: crate::tree::ProjectedChildTurnUsage) {
            self.0.lock().unwrap().push(terminal);
        }
        fn node_spawned(&self, _: &str, _: &str, _: Option<&str>, _: &str) {}
        fn node_detached(&self, _: &str, _: &str, _: Option<&str>, _: crate::tree::NodeDetach) {}
        fn epoch_advanced(&self, _: &str) {}
    }

    #[test]
    fn release_locator_is_derived_from_its_feed_not_its_returned_locator() {
        use crate::protocol::SpawnAgentRequest;
        use crate::tree::{TreeFeed, TreeFeedReads};
        use plakous::sessions::feed::{FeedFact, FeedHeader, FeedRecord, FeedTurnOutcome};

        let root = tempfile::tempdir().unwrap();
        let observer = Arc::new(UsageObserver::default());
        let runtime = ProductTopologyRuntime::new(
            bootstrap("release-feed", root.path().to_path_buf()),
            memory_store(),
            Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
            Some(observer.clone()),
        );
        let session = "00000000-0000-4000-8000-0000000000c1";
        let branch = "00000000-0000-4000-8000-00000000000b";
        let head = "00000000-0000-4000-8000-000000003001";
        let locator = format!("{session}/{branch}/{head}");
        let child = runtime
            .mutate_then_emit(false, |tree| {
                let child = tree
                    .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                    .map_err(|error| error.message)?
                    .agent_id;
                tree.record_child_session(&child, &locator)
                    .map_err(|error| error.message)?;
                Ok(child)
            })
            .unwrap();
        publish_release(
            runtime.driver(),
            &child,
            LiveRelease {
                locator: Some("not-feed-authority".to_owned()),
                ..LiveRelease::default()
            },
        )
        .unwrap();
        runtime
            .read(|tree| assert!(tree.node(&child).unwrap().session_locator.is_none()))
            .unwrap();

        let header: FeedHeader = serde_json::from_value(serde_json::json!({
            "session_id": session,
            "seq": 1,
            "branch_id": branch,
            "committed_at_ms": 1000,
            "change": {"lease_released": {"head": head, "fence": 1}},
        }))
        .unwrap();
        let feed = TreeFeed {
            session_id: session.to_owned(),
            retained_from: 1,
            below_retained: None,
            heads: Vec::new(),
            head_turns: std::collections::BTreeMap::default(),
            records: vec![FeedRecord::from_store(
                header,
                vec![FeedFact::TurnTerminal {
                    turn_id: "00000000-0000-4000-8000-000000001001".parse().unwrap(),
                    command_id: CommandId::new("release-usage").unwrap(),
                    outcome: FeedTurnOutcome::Completed {
                        finish: plakous::model::FinishReason::Stop,
                    },
                    closed_exchange: true,
                    steering_settled_unapplied: false,
                    usage: plakous::agent::TurnUsage::new(
                        plakous::model::Usage::new(123, 45, 0, 0, 0),
                        1,
                        1,
                        0,
                    )
                    .unwrap(),
                }],
            )],
            reads: TreeFeedReads::default(),
        };
        publish_release(
            runtime.driver(),
            &child,
            LiveRelease {
                locator: Some("still-not-feed-authority".to_owned()),
                feed: Some(feed.clone()),
                failure: None,
            },
        )
        .unwrap();
        let emitted = observer.0.lock().unwrap().clone();
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].usage.model().input_tokens(), 123);
        assert_eq!(emitted[0].committed_at_ms, 1000);
        publish_release(
            runtime.driver(),
            &child,
            LiveRelease {
                feed: Some(feed.clone()),
                ..LiveRelease::default()
            },
        )
        .unwrap();
        assert_eq!(*observer.0.lock().unwrap(), emitted);
        runtime
            .read(|tree| {
                assert_eq!(
                    tree.node(&child).unwrap().session_locator.as_deref(),
                    Some(locator.as_str())
                );
                assert_eq!(tree.feed_cursor(session), 1);
                let mut rebuilt = tree.clone();
                rebuilt.clear_feed_views();
                assert_eq!(feed.apply(&mut rebuilt, &child).unwrap().applied, 1);
                assert_eq!(
                    rebuilt.node(&child).unwrap().session_locator,
                    tree.node(&child).unwrap().session_locator
                );
                assert_eq!(feed.apply(&mut rebuilt, &child).unwrap().applied, 0);
            })
            .unwrap();
    }

    #[tokio::test]
    async fn lookup_returns_the_same_tree_arc() {
        let tree_id = unique_tree_id();
        let root = tempfile::tempdir().unwrap();
        let first = insert_root(
            tree_id.clone(),
            bootstrap(&tree_id, root.path().to_path_buf()),
            None,
        )
        .unwrap();
        let second = lookup(&tree_id).expect("hub should occupy after insert");
        assert!(Arc::ptr_eq(&first.tree, &second.tree));
        assert!(Arc::ptr_eq(&first.live, &second.live));
        force_release(&tree_id).await;
        assert!(lookup(&tree_id).is_none());
    }

    /// A hub hit — root or child, attaching or not
    /// — asks for no store and loads nothing.
    #[tokio::test]
    async fn hub_hit_asks_for_no_store() {
        let root = tempfile::tempdir().unwrap();
        test_support::hub_hit_opens_no_store(root.path().to_path_buf()).await;
    }

    #[tokio::test]
    async fn child_resolve_does_not_load_or_pause() {
        let tree_id = unique_tree_id();
        let root = tempfile::tempdir().unwrap();
        let runtime = insert_root(
            tree_id.clone(),
            bootstrap(&tree_id, root.path().to_path_buf()),
            None,
        )
        .unwrap();
        let generation = runtime.tree.lock().unwrap().host_generation;
        let state = runtime.tree.lock().unwrap().state;
        let loaded = AtomicBool::new(false);
        let resolved = resolve_topology_runtime(
            &tree_id,
            false,
            TopologyHubAttach::LookupOnly,
            || Ok(memory_store()),
            None,
            |_| {
                loaded.store(true, Ordering::SeqCst);
                Err(TreeRuntimeError::new("child compile must not load_tree"))
            },
        )
        .unwrap();
        assert!(!loaded.load(Ordering::SeqCst));
        assert!(Arc::ptr_eq(&runtime.tree, &resolved.tree));
        {
            let tree = resolved.tree.lock().unwrap();
            assert_eq!(tree.host_generation, generation);
            assert_eq!(tree.state, state);
        }
        force_release(&tree_id).await;
    }

    #[tokio::test]
    async fn missing_hub_child_fails_closed_without_bootstrap() {
        let tree_id = unique_tree_id();
        let loaded = AtomicBool::new(false);
        let error = resolve_topology_runtime(
            &tree_id,
            false,
            TopologyHubAttach::LookupOnly,
            || Ok(memory_store()),
            None,
            |_| {
                loaded.store(true, Ordering::SeqCst);
                Err(TreeRuntimeError::new("must not bootstrap"))
            },
        )
        .err()
        .expect("child compile without hub must fail closed");
        assert!(!loaded.load(Ordering::SeqCst));
        assert!(
            error
                .message()
                .contains("child catalog compile cannot load")
        );
        assert!(lookup(&tree_id).is_none());
    }

    #[tokio::test]
    async fn missing_hub_root_lookup_does_not_bootstrap() {
        let tree_id = unique_tree_id();
        let loaded = AtomicBool::new(false);
        let error = resolve_topology_runtime(
            &tree_id,
            true,
            TopologyHubAttach::LookupOnly,
            || Ok(memory_store()),
            None,
            |_| {
                loaded.store(true, Ordering::SeqCst);
                Err(TreeRuntimeError::new("must not bootstrap"))
            },
        )
        .err()
        .expect("root lookup-only without hub must fail closed");
        assert!(!loaded.load(Ordering::SeqCst));
        assert!(error.message().contains("outside session attach"));
        assert!(lookup(&tree_id).is_none());
    }

    #[tokio::test]
    async fn missing_hub_child_does_not_clobber_durable_tree() {
        let tree_id = unique_tree_id();
        let root = tempfile::tempdir().unwrap();
        let store = memory_store();
        let mut tree = bootstrap(&tree_id, root.path().to_path_buf());
        tree.spawn_agent(
            "root",
            &crate::protocol::SpawnAgentRequest::test_review(),
            None,
        )
        .unwrap();
        tree.acknowledge_revision(store.save(&tree).unwrap());

        let error = resolve_topology_runtime(
            &tree_id,
            false,
            TopologyHubAttach::AttachRoot,
            || Ok(memory_store()),
            None,
            |_| Ok(bootstrap(&tree_id, root.path().to_path_buf())),
        )
        .err()
        .expect("child miss must not bootstrap even when attach is requested");
        assert!(
            error
                .message()
                .contains("child catalog compile cannot load")
        );

        let loaded = ProductAgentTree::reopen(store.load(&tree_id).unwrap().unwrap());
        assert!(loaded.node("c1").is_ok());
    }

    #[tokio::test]
    async fn attach_root_load_runs_once_then_lookup_shares() {
        let tree_id = unique_tree_id();
        let root = tempfile::tempdir().unwrap();
        let loads = AtomicUsize::new(0);
        let first = resolve_topology_runtime(
            &tree_id,
            true,
            TopologyHubAttach::AttachRoot,
            || Ok(memory_store()),
            None,
            |_| {
                loads.fetch_add(1, Ordering::SeqCst);
                Ok(bootstrap(&tree_id, root.path().to_path_buf()))
            },
        )
        .unwrap();
        let second = resolve_topology_runtime(
            &tree_id,
            true,
            TopologyHubAttach::AttachRoot,
            || Ok(memory_store()),
            None,
            |_| {
                loads.fetch_add(1, Ordering::SeqCst);
                Err(TreeRuntimeError::new("second attach must not reload"))
            },
        )
        .unwrap();
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert!(Arc::ptr_eq(&first.tree, &second.tree));
        force_release(&tree_id).await;
    }

    #[tokio::test]
    async fn attach_root_resolve_acquires_one_lease() {
        let tree_id = unique_tree_id();
        let root = tempfile::tempdir().unwrap();
        resolve_topology_runtime(
            &tree_id,
            true,
            TopologyHubAttach::AttachRoot,
            || Ok(memory_store()),
            None,
            |_| Ok(bootstrap(&tree_id, root.path().to_path_buf())),
        )
        .unwrap();
        release_root_lease(&tree_id).await.unwrap();
        assert!(lookup(&tree_id).is_none());
    }

    #[tokio::test]
    async fn overlapping_root_leases_keep_the_hub() {
        let tree_id = unique_tree_id();
        let root = tempfile::tempdir().unwrap();
        insert_root(
            tree_id.clone(),
            bootstrap(&tree_id, root.path().to_path_buf()),
            None,
        )
        .unwrap();
        process_hub().acquire_root_lease(&tree_id).unwrap();
        process_hub().acquire_root_lease(&tree_id).unwrap();
        release_root_lease(&tree_id).await.unwrap();
        assert!(lookup(&tree_id).is_some());
        release_root_lease(&tree_id).await.unwrap();
        assert!(lookup(&tree_id).is_none());
    }

    #[tokio::test]
    async fn child_release_does_not_drop_hub() {
        let tree_id = unique_tree_id();
        let root = tempfile::tempdir().unwrap();
        insert_root(
            tree_id.clone(),
            bootstrap(&tree_id, root.path().to_path_buf()),
            None,
        )
        .unwrap();
        process_hub().acquire_root_lease(&tree_id).unwrap();
        release_unleased(&tree_id).await;
        assert!(lookup(&tree_id).is_some());
        release_root_lease(&tree_id).await.unwrap();
        assert!(lookup(&tree_id).is_none());
    }

    #[tokio::test]
    async fn failed_attach_release_unleased_removes_hub() {
        let tree_id = unique_tree_id();
        let root = tempfile::tempdir().unwrap();
        insert_root(
            tree_id.clone(),
            bootstrap(&tree_id, root.path().to_path_buf()),
            None,
        )
        .unwrap();
        release_unleased(&tree_id).await;
        assert!(lookup(&tree_id).is_none());
    }

    /// Every child cancel is bounded by the same settle window as the root's.
    ///
    /// `TurnControl::cancel` waits until the runtime settles, so a bare `await`
    /// on it lets a child whose provider ignores the token hang stop-all and
    /// the host loop with it. A `TurnControl` cannot be
    /// constructed offline, so the law is pinned at the source: this module has
    /// no unbounded `control.cancel()` await left.
    #[test]
    fn child_cancels_are_bounded_by_the_host_settle_timeout() {
        let source = include_str!("hub.rs");
        assert!(
            source.contains("plakous_host_protocol::limits::HOST_CANCEL_SETTLE_TIMEOUT"),
            "the child cancel must go through the shared settle window"
        );
        assert!(
            !source.contains(".cancel()\n            .await"),
            "an unbounded child cancel await is back"
        );
    }

    #[tokio::test]
    async fn cancel_live_missing_agent_is_idempotent() {
        let tree_id = unique_tree_id();
        let root = tempfile::tempdir().unwrap();
        let runtime = insert_root(
            tree_id.clone(),
            bootstrap(&tree_id, root.path().to_path_buf()),
            None,
        )
        .unwrap();
        // Nothing was ever live here, so every cancel is `NotLive` — not a
        // failure, and not a cancel either. Folding both into `Ok(())` is
        // what hid the difference from the close path.
        assert_eq!(
            runtime.cancel_live("c1").await.unwrap(),
            LiveCancelOutcome::NotLive
        );
        let report = runtime.cancel_live_many(&["c1".into(), "c2".into()]).await;
        assert!(report.is_clean(), "a missing agent is not a failure");
        assert!(report.cancelled.is_empty(), "nothing was running to stop");
        assert_eq!(report.not_live, vec!["c1".to_owned(), "c2".to_owned()]);
        force_release(&tree_id).await;
    }

    #[tokio::test]
    async fn crash_cut_reattach_is_single_flight() {
        let tree_id = unique_tree_id();
        let root = tempfile::tempdir().unwrap();
        let runtime = insert_root(
            tree_id.clone(),
            bootstrap(&tree_id, root.path().to_path_buf()),
            None,
        )
        .unwrap();
        let held = runtime.lock_reattach().await;
        assert!(runtime.try_lock_reattach().is_err());
        let shared = lookup(&tree_id).expect("hub remains");
        assert!(shared.try_lock_reattach().is_err());
        drop(held);
        assert!(runtime.try_lock_reattach().is_ok());
        force_release(&tree_id).await;
    }

    /// Quit releases every child of the tree in one bounded wait, not
    /// one after another. Timing-free: the release futures are handed to a
    /// single `join_all` under one timeout, so the teardown's worst case is
    /// the slowest child rather than their sum.
    /// The live table keeps the same invariant when handed
    /// to the product: the closed flag and the live map are one lock, so a
    /// turn racing a shutdown is either in the set the drain takes or refused
    /// by the flag the drain already set — never left running with nothing
    /// holding it.
    ///
    /// Driven against the real map: the test holds the live lock, which is
    /// exactly the window a racing shutdown would try to use, and asserts the
    /// decision flips under it.
    #[tokio::test]
    async fn a_turn_racing_shutdown_is_drained_or_refused_but_never_orphaned() {
        let tree_id = unique_tree_id();
        let runtime = insert_root(
            tree_id.clone(),
            bootstrap(&tree_id, std::env::temp_dir()),
            None,
        )
        .unwrap();

        {
            let live = runtime.live.lock().await;
            assert!(
                runtime.admits_live(&live, "c1"),
                "an idle agent on a live tree is admitted"
            );
        }

        // The shutdown sets the flag before it takes the lock to drain. Hold
        // the lock across the flag so the racing window is real, then assert
        // the decision a racing admission would take inside it.
        let held = runtime.live.lock().await;
        runtime.close_for_shutdown();
        assert!(
            !runtime.admits_live(&held, "c1"),
            "an admission that wins the lock after the flag is set must refuse"
        );
        drop(held);

        assert!(
            runtime.live.lock().await.is_empty(),
            "no child was admitted before shutdown"
        );
        assert!(
            runtime.is_closed(),
            "the flag stays set: nothing is admitted after a drain"
        );
        force_release(&tree_id).await;
    }

    /// The closed flag is not consulted beside the map: `try_occupy_live`
    /// re-reads it *inside* the lock it inserts under, so a check that
    /// happened before the lock cannot be the one the insert relies on.
    #[test]
    fn the_closed_flag_is_read_under_the_lock_the_insert_takes() {
        let source = include_str!("hub.rs");
        let admits = source
            .split("fn admits_live(")
            .nth(1)
            .expect("admits_live is the one admission decision");
        let body = &admits[..admits.find("\n    }\n").unwrap_or(admits.len())];
        assert!(
            body.contains("self.is_closed()") && body.contains("live.contains_key"),
            "the admission reads the flag and the map together"
        );
        let occupy = source
            .split("pub async fn try_occupy_live_admitted(")
            .nth(1)
            .expect("try_occupy_live_admitted is the admission entry");
        let occupy = &occupy[..occupy.find("\n    }\n").unwrap_or(occupy.len())];
        let lock_at = occupy
            .find("self.live.lock()")
            .expect("the admission takes the live lock");
        let decide_at = occupy
            .find("self.admits_live(")
            .expect("the admission goes through the one decision");
        assert!(
            lock_at < decide_at,
            "the decision must be taken with the live lock held"
        );
        let insert_at = occupy
            .find("live.children.insert(")
            .expect("the admission inserts");
        assert!(
            decide_at < insert_at,
            "the decision must precede the insert it guards"
        );
    }

    struct RefusingStore;
    struct AttachProbeStore {
        hub: std::sync::Weak<TopologyHub>,
        inner: crate::tree::MemoryTreeStore,
        saves: AtomicUsize,
    }

    impl TreeStore for AttachProbeStore {
        fn exists(&self, id: &str) -> Result<bool, crate::tree::TreeStoreError> {
            self.inner.exists(id)
        }
        fn ids(&self) -> Result<Vec<String>, crate::tree::TreeStoreError> {
            self.inner.ids()
        }
        fn load(
            &self,
            id: &str,
        ) -> Result<Option<crate::tree::PersistedTree>, crate::tree::TreeStoreError> {
            self.inner.load(id)
        }
        fn save(
            &self,
            tree: &ProductAgentTree,
        ) -> Result<crate::tree::Revision, crate::tree::TreeStoreError> {
            assert!(
                self.hub.upgrade().unwrap().entries.try_lock().is_ok(),
                "attach saved under the hub entries lock"
            );
            self.saves.fetch_add(1, Ordering::SeqCst);
            self.inner.save(tree)
        }
    }

    #[test]
    fn attach_startup_is_unlocked_and_skips_an_empty_consequence() {
        let root = tempfile::tempdir().unwrap();
        let hub = Arc::new(TopologyHub::new());
        let store = Arc::new(AttachProbeStore {
            hub: Arc::downgrade(&hub),
            inner: crate::tree::MemoryTreeStore::new(),
            saves: AtomicUsize::new(0),
        });
        let vcs = Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new());
        let tree = bootstrap("attach-probe", root.path().to_path_buf());
        let runtime = hub
            .insert_root(
                "attach-probe",
                tree.clone(),
                store.clone(),
                vcs.clone(),
                None,
            )
            .unwrap();
        assert_eq!(store.saves.load(Ordering::SeqCst), 1);
        let occupant = hub
            .insert_root("attach-probe", tree, store.clone(), vcs, None)
            .unwrap();
        assert!(Arc::ptr_eq(&runtime, &occupant));
        assert_eq!(store.saves.load(Ordering::SeqCst), 1);
    }

    impl TreeStore for RefusingStore {
        fn exists(&self, _: &str) -> Result<bool, crate::tree::TreeStoreError> {
            Ok(false)
        }
        fn ids(&self) -> Result<Vec<String>, crate::tree::TreeStoreError> {
            Ok(vec![])
        }
        fn save(
            &self,
            _: &ProductAgentTree,
        ) -> Result<crate::tree::Revision, crate::tree::TreeStoreError> {
            Err(crate::tree::TreeStoreError::unavailable(
                "initial publish refused",
            ))
        }
        fn load(
            &self,
            _: &str,
        ) -> Result<Option<crate::tree::PersistedTree>, crate::tree::TreeStoreError> {
            Ok(None)
        }
    }

    #[test]
    fn initial_publish_refusal_does_not_mutate_or_dispatch_queued_seal() {
        use crate::protocol::{
            AttemptScopedRequest, NodeStatus, SpawnAgentRequest, Tier, WriterMode,
        };
        use plakous_workspace::occupancy_vcs::fake::FakeVcs;
        let root = tempfile::tempdir().unwrap();
        let vcs = Arc::new(FakeVcs::new());
        let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
            vcs.clone(),
            "refused-seal",
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let mut binding = crate::epoch::RosterBinding::leaf("test-account", "selected-tuple");
        binding.max_tier = Tier::Mutate;
        tree.pin_template("implement", binding);
        let child = tree
            .spawn_agent(
                "root",
                &SpawnAgentRequest::test_named("implement", Tier::Mutate, "impl", "edit")
                    .with_fork(),
                None,
            )
            .unwrap()
            .agent_id;
        let runtime = ProductTopologyRuntime::new(tree, Arc::new(RefusingStore), vcs.clone(), None);
        let error = runtime
            .mutate_then_emit(false, |tree| {
                tree.close_agent(
                    "root",
                    &AttemptScopedRequest {
                        agent_id: child.clone(),
                        expected_attempt_id: None,
                    },
                    None,
                )
                .map_err(|error| error.message)
            })
            .unwrap_err();
        assert!(error.contains("initial publish refused"));
        let mut tree = runtime.tree.lock().unwrap();
        assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Running);
        assert!(tree.attempt(&child).unwrap().candidate_id.is_none());
        assert!(
            vcs.calls_of(plakous_workspace::occupancy_vcs::fake::FakeOp::Seal)
                .is_empty()
        );
        assert!(
            tree.take_effects().is_empty(),
            "no cleanup may remain stranded"
        );
    }

    #[test]
    fn initial_publish_refusal_dispatches_no_hook_or_removal() {
        use crate::protocol::{NodeStatus, SpawnAgentRequest, Tier, WriterMode};
        use plakous_workspace::occupancy_vcs::{
            VcsError,
            fake::{FakeOp, FakeVcs},
        };
        let root = tempfile::tempdir().unwrap();
        let vcs = Arc::new(FakeVcs::new());
        let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
            vcs.clone(),
            "refused-hook",
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let mut binding = crate::epoch::RosterBinding::leaf("test-account", "selected-tuple");
        binding.max_tier = Tier::Mutate;
        binding.on_mint = Some("fixture-hook".into());
        tree.pin_template("implement", binding);
        let child = tree
            .spawn_agent(
                "root",
                &SpawnAgentRequest::test_named("implement", Tier::Mutate, "impl", "edit")
                    .with_fork(),
                None,
            )
            .unwrap()
            .agent_id;
        let path = tree.node(&child).unwrap().path.clone();
        vcs.hook_next_with(Err(VcsError::hook_failed("fixture failure")));
        let runtime = ProductTopologyRuntime::new(tree, Arc::new(RefusingStore), vcs.clone(), None);
        assert!(
            runtime
                .mutate_then_emit(false, |_| Ok(()))
                .unwrap_err()
                .contains("initial publish refused")
        );
        assert!(vcs.calls_of(FakeOp::Hook).is_empty());
        assert!(vcs.calls_of(FakeOp::Unmint).is_empty());
        assert!(path.exists());
        let mut tree = runtime.tree.lock().unwrap();
        assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Running);
        assert!(
            !tree.take_effects().is_empty(),
            "unpublished hook remains queued"
        );
    }

    #[test]
    fn effect_free_mutation_publishes_acknowledged_revision() {
        let root = tempfile::tempdir().unwrap();
        let store = memory_store();
        let runtime = ProductTopologyRuntime::new(
            bootstrap("effect-free", root.path().to_path_buf()),
            store.clone(),
            Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
            None,
        );
        runtime
            .mutate_then_emit(false, |tree| {
                tree.record_fault("root", "durable fault");
                Ok(())
            })
            .unwrap();
        assert!(store.load("effect-free").unwrap().is_some());
        runtime
            .read(|tree| {
                assert_eq!(tree.revision, tree.stored_revision);
                assert_eq!(tree.fault_reason("root"), Some("durable fault"));
            })
            .unwrap();
    }

    struct BarrierChild {
        panic_on_release: bool,
        failure: Option<String>,
        turn: TurnId,
        entered: Arc<tokio::sync::Notify>,
        finish: Arc<tokio::sync::Notify>,
    }
    impl LiveChildHandle for BarrierChild {
        fn agent(&self) -> Option<Agent> {
            None
        }
        fn turn_id(&self) -> TurnId {
            self.turn
        }
        fn turn_control(&self) -> TurnControl {
            panic!("control is not used by this fixture")
        }
        fn follow_up(
            &self,
            _: CommandId,
            _: String,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> {
            Box::pin(async { Err("fixture".into()) })
        }

        fn follow_up_barrier(&self) -> Option<Arc<tokio::sync::RwLock<()>>> {
            None
        }
        fn restart(
            &mut self,
            _: TreeDriver,
            _: CommandId,
            _: String,
            _delivery: Option<tokio::sync::OwnedRwLockWriteGuard<()>>,
        ) -> Pin<Box<dyn Future<Output = Result<TurnResultHandle, String>> + Send + '_>> {
            Box::pin(async { Err("fixture".into()) })
        }
        fn release(
            self: Box<Self>,
            driver: TreeDriver,
        ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>> {
            self.cancel_and_release(driver)
        }
        fn cancel_and_release(
            self: Box<Self>,
            _driver: TreeDriver,
        ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>> {
            Box::pin(async move {
                self.entered.notify_one();
                self.finish.notified().await;
                assert!(!self.panic_on_release, "injected child release panic");
                LiveRelease {
                    feed: None,
                    locator: Some("released-locator".into()),
                    failure: self.failure,
                }
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn joiner_deadline_does_not_refault_a_settled_release() {
        use crate::protocol::{AttemptStatus, SpawnAgentRequest};
        use crate::tree::RecoveryResolution;
        use std::task::{Context, Poll};

        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(ProductTopologyRuntime::new(
            bootstrap("joiner-race", root.path().to_path_buf()),
            memory_store(),
            Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
            None,
        ));
        let id = runtime
            .mutate_then_emit(false, |tree| {
                tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                    .map(|child| child.agent_id)
                    .map_err(|error| error.message)
            })
            .unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        assert!(
            runtime
                .try_occupy_live(
                    id.clone(),
                    Box::new(BarrierChild {
                        panic_on_release: false,
                        failure: None,
                        turn: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                        entered: entered.clone(),
                        finish: finish.clone(),
                    })
                )
                .await
                .is_ok()
        );
        let owner = runtime.clone();
        let child = id.clone();
        let joiner = Box::pin(async move { owner.cancel_live(&child).await });
        let joiner = Arc::new(Mutex::new(Some(joiner)));
        let worker_joiner = joiner.clone();
        let weak = Arc::downgrade(&runtime);
        let child = id.clone();
        runtime
            .set_retirement_recovery(Arc::new(move || {
                let pending = joiner.lock().unwrap().take();
                let Some(mut pending) = pending else {
                    return;
                };
                let runtime = weak.upgrade().unwrap();
                runtime
                    .mutate_then_emit(false, |tree| {
                        let attempt = tree.attempt(&child).unwrap().attempt_id.clone();
                        tree.recover_host_decision(
                            &child,
                            &attempt,
                            RecoveryResolution::RecoveredWithoutTurn {
                                message: "quiescent".into(),
                            },
                        )
                        .map_err(|error| error.message)
                    })
                    .unwrap();
                // Release is published and settled, but the worker has not returned
                // its shared completion yet. Poll the expired joiner in this gap.
                let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());
                assert!(matches!(
                    pending.as_mut().poll(&mut cx),
                    Poll::Ready(Err(_))
                ));
            }))
            .await;
        {
            let mut joiner = worker_joiner.lock().unwrap();
            let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());
            assert!(joiner.as_mut().unwrap().as_mut().poll(&mut cx).is_pending());
        }
        entered.notified().await;
        let completion = runtime.live.lock().await.retiring[&id].completion.clone();
        tokio::time::advance(HOST_RETIRE_JOIN_TIMEOUT).await;
        tokio::task::yield_now().await;
        runtime
            .read(|tree| {
                assert_eq!(
                    tree.attempt(&id).unwrap().status,
                    AttemptStatus::RecoveryRequired
                );
            })
            .unwrap();
        finish.notify_one();
        completion.await.unwrap();
        runtime
            .read(|tree| {
                assert_eq!(tree.attempt(&id).unwrap().status, AttemptStatus::Failed);
                assert_eq!(
                    tree.node(&id).unwrap().status,
                    crate::protocol::NodeStatus::Failed
                );
            })
            .unwrap();
    }

    /// A completed child's release can join the runtime's own store-bounded
    /// shutdown work (an in-flight store call, then the shutdown receipt).
    /// A release slower than a cancel settle but within that bound is not a
    /// failure: close waits for it, records no fault and keeps the terminal.
    #[tokio::test(start_paused = true)]
    async fn slow_store_bounded_release_completes_close_without_a_fault() {
        use crate::protocol::{AttemptStatus, NodeStatus, SpawnAgentRequest, SubmitResultRequest};
        let root = tempfile::tempdir().unwrap();
        let mut tree = bootstrap("slow-release", root.path().to_path_buf());
        let id = tree
            .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
            .unwrap()
            .agent_id;
        tree.submit_result(
            &id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed(&id).unwrap();
        tree.settle_for_test();
        let runtime = Arc::new(ProductTopologyRuntime::new(
            tree,
            memory_store(),
            Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
            None,
        ));
        let recoveries = Arc::new(AtomicUsize::new(0));
        let triggered = recoveries.clone();
        runtime
            .set_retirement_recovery(Arc::new(move || {
                triggered.fetch_add(1, Ordering::SeqCst);
            }))
            .await;
        let entered = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        assert!(
            runtime
                .try_occupy_live(
                    id.clone(),
                    Box::new(BarrierChild {
                        panic_on_release: false,
                        failure: None,
                        turn: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                        entered: entered.clone(),
                        finish: finish.clone(),
                    })
                )
                .await
                .is_ok()
        );
        let owner = runtime.clone();
        let child = id.clone();
        let mut caller = tokio::spawn(async move { owner.cancel_live(&child).await });
        entered.notified().await;
        // Past a cancel settle plus process-exit release, within the
        // runtime's store-bounded shutdown.
        tokio::time::advance(
            plakous_host_protocol::limits::HOST_CANCEL_SETTLE_TIMEOUT
                + HOST_SHUTDOWN_RELEASE_TIMEOUT
                + Duration::from_secs(13),
        )
        .await;
        tokio::task::yield_now().await;
        assert!(
            tokio::time::timeout(Duration::ZERO, &mut caller)
                .await
                .is_err(),
            "close waits for a release still within its bound"
        );
        runtime
            .read(|tree| {
                assert_eq!(tree.fault_reason(&id), None);
                assert_eq!(tree.node(&id).unwrap().status, NodeStatus::Completed);
            })
            .unwrap();
        finish.notify_one();
        assert_eq!(caller.await.unwrap(), Ok(LiveCancelOutcome::Cancelled));
        assert_eq!(recoveries.load(Ordering::SeqCst), 0);
        runtime
            .read(|tree| {
                assert_eq!(tree.fault_reason(&id), None);
                assert_eq!(tree.node(&id).unwrap().status, NodeStatus::Completed);
                assert_ne!(
                    tree.attempt(&id).unwrap().status,
                    AttemptStatus::RecoveryRequired
                );
            })
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn fast_clean_retirement_does_not_trigger_recovery() {
        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(ProductTopologyRuntime::new(
            bootstrap("clean-retirement", root.path().to_path_buf()),
            memory_store(),
            Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
            None,
        ));
        let calls = Arc::new(AtomicUsize::new(0));
        let triggered = calls.clone();
        runtime
            .set_retirement_recovery(Arc::new(move || {
                triggered.fetch_add(1, Ordering::SeqCst);
            }))
            .await;
        let entered = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        finish.notify_one();
        assert!(
            runtime
                .try_occupy_live(
                    "root".into(),
                    Box::new(BarrierChild {
                        panic_on_release: false,
                        failure: None,
                        turn: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                        entered,
                        finish,
                    })
                )
                .await
                .is_ok()
        );
        runtime.cancel_live("root").await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn late_retirement_clears_slot_and_publishes_locator() {
        late_retirement(None).await;
    }

    #[tokio::test]
    async fn completed_terminal_does_not_hide_its_owners_release_failure() {
        use crate::protocol::{NodeStatus, SpawnAgentRequest, SubmitResultRequest};
        let root = tempfile::tempdir().unwrap();
        let mut tree = bootstrap("completed-release-failure", root.path().to_path_buf());
        let id = tree
            .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
            .unwrap()
            .agent_id;
        tree.submit_result(
            &id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed(&id).unwrap();
        tree.settle_for_test();
        assert_eq!(tree.node(&id).unwrap().status, NodeStatus::Completed);
        let runtime = Arc::new(ProductTopologyRuntime::new(
            tree,
            memory_store(),
            Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
            None,
        ));
        let entered = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        assert!(
            runtime
                .try_occupy_live(
                    id.clone(),
                    Box::new(BarrierChild {
                        panic_on_release: false,
                        failure: Some("owner release failed".into()),
                        turn: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                        entered: entered.clone(),
                        finish: finish.clone(),
                    })
                )
                .await
                .is_ok()
        );
        finish.notify_one();
        assert!(matches!(
            runtime.cancel_live(&id).await,
            Err(LiveCancelError::OwnerFailure(_))
        ));
        runtime
            .read(|tree| {
                assert_eq!(tree.node(&id).unwrap().status, NodeStatus::RecoveryRequired);
                assert!(matches!(
                    tree.recovery_need(&id).unwrap(),
                    crate::tree::RecoveryNeed::Session { .. }
                ));
            })
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn startup_join_deadline_records_its_stage() {
        use crate::protocol::SpawnAgentRequest;
        let root = tempfile::tempdir().unwrap();
        let runtime = ProductTopologyRuntime::new(
            bootstrap("startup-deadline-stage", root.path().to_path_buf()),
            memory_store(),
            Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
            None,
        );
        let id = runtime
            .mutate_then_emit(false, |tree| {
                tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                    .map(|child| child.agent_id)
                    .map_err(|error| error.message)
            })
            .unwrap();
        let startup = runtime.reserve_startup(&id).await.unwrap();
        startup.set_stage(ChildStartupStage::Releasing);
        assert_eq!(
            runtime.cancel_live(&id).await,
            Err(LiveCancelError::JoinExpired)
        );
        runtime
            .read(|tree| {
                let message = &tree.attempt(&id).unwrap().failure.as_ref().unwrap().message;
                assert!(message.contains("startup"), "{message}");
                assert!(message.contains("Releasing"), "{message}");
            })
            .unwrap();
        runtime
            .finish_startup(
                &id,
                &startup,
                LiveRelease {
                    feed: None,
                    locator: None,
                    failure: None,
                },
            )
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn owner_release_failure_fences_unstarted_followup() {
        use crate::protocol::{
            AttemptStatus, MessagePurpose, NodeStatus, SendAgentMessageRequest, SpawnAgentRequest,
            SubmitResultRequest,
        };
        const SESSION: &str = "00000000-0000-4000-8000-000000000100";
        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(ProductTopologyRuntime::new(
            bootstrap("followup-release-failure", root.path().to_path_buf()),
            memory_store(),
            Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
            None,
        ));
        let id = runtime
            .mutate_then_emit(false, |tree| {
                let id = tree
                    .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                    .unwrap()
                    .agent_id;
                tree.record_child_session(&id,
                    "00000000-0000-4000-8000-000000000100/00000000-0000-4000-8000-000000000200/00000000-0000-4000-8000-000000000300").unwrap();
                tree.fold_due(&id).unwrap();
                tree.submit_result(
                    &id,
                    &SubmitResultRequest {
                        turn_token: 1,
                        output_json: r#"{"ok":true}"#.into(),
                    },
                    None,
                )
                .unwrap();
                tree.record_turn_completed(&id).unwrap();
                Ok(id)
            })
            .unwrap();
        let first = runtime
            .read(|tree| tree.attempt(&id).unwrap().attempt_id.clone())
            .unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        assert!(
            runtime
                .try_occupy_live(
                    id.clone(),
                    Box::new(BarrierChild {
                        panic_on_release: false,
                        failure: Some("owner release failed".into()),
                        turn: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                        entered: entered.clone(),
                        finish: finish.clone(),
                    })
                )
                .await
                .is_ok()
        );
        let owner = runtime.clone();
        let child = id.clone();
        let cancel = tokio::spawn(async move { owner.cancel_live(&child).await });
        entered.notified().await;
        let mut opener = String::new();
        runtime
            .mutate_then_emit(false, |tree| {
                opener = tree
                    .send_agent_message(
                        "root",
                        &SendAgentMessageRequest {
                            agent_id: id.clone(),
                            message: "again".into(),
                            purpose: Some(MessagePurpose::Followup),
                            priority: None,
                            in_reply_to: None,
                            output_schema_json: None,
                        },
                        None,
                    )
                    .map_err(|error| error.message)?
                    .message_id;
                assert_ne!(tree.attempt(&id).unwrap().attempt_id, first);
                assert!(tree.attempt(&id).unwrap().turn_id.is_none());
                let mut without_release_failure = tree.clone();
                assert!(
                    without_release_failure
                        .withdraw_unstarted_followup(&id, &opener, SESSION, 0)
                        .is_ok()
                );
                Ok(())
            })
            .unwrap();
        assert!(runtime.reserve_startup(&id).await.is_err());
        finish.notify_one();
        assert!(matches!(
            cancel.await.unwrap(),
            Err(LiveCancelError::OwnerFailure(_))
        ));
        assert!(!runtime.live.lock().await.contains_key(&id));
        runtime
            .mutate_then_emit(false, |tree| {
                assert!(
                    tree.withdraw_unstarted_followup(&id, &opener, SESSION, 0)
                        .is_err(),
                    "owner evidence must prevent silent rollback"
                );
                assert_eq!(tree.node(&id).unwrap().status, NodeStatus::RecoveryRequired);
                let attempt = tree.attempt(&id).unwrap();
                assert_eq!(attempt.status, AttemptStatus::RecoveryRequired);
                assert_eq!(attempt.failure.as_ref().unwrap().kind, "session_retirement");
                assert!(
                    attempt
                        .failure
                        .as_ref()
                        .unwrap()
                        .message
                        .contains("owner release failed")
                );
                assert!(matches!(
                    tree.recovery_need(&id).unwrap(),
                    crate::tree::RecoveryNeed::Session { .. }
                ));
                tree.record_host_materialization(&id)
                    .and_then(|value| tree.fold_due(&id).map(|_| value))
                    .map_err(|error| error.message)?;
                assert_eq!(tree.node(&id).unwrap().status, NodeStatus::RecoveryRequired);
                assert!(
                    tree.attempt(&id)
                        .unwrap()
                        .failure
                        .as_ref()
                        .unwrap()
                        .message
                        .contains("owner release failed")
                );
                Ok(())
            })
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn late_failing_retirement_clears_slot_and_publishes_locator() {
        late_retirement(Some("injected cleanup failure".into())).await;
    }

    async fn late_retirement(failure: Option<String>) {
        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(ProductTopologyRuntime::new(
            bootstrap("late-retirement", root.path().to_path_buf()),
            memory_store(),
            Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
            None,
        ));
        let entered = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        let recoveries = Arc::new(AtomicUsize::new(0));
        let triggered = recoveries.clone();
        runtime
            .set_retirement_recovery(Arc::new(move || {
                triggered.fetch_add(1, Ordering::SeqCst);
            }))
            .await;
        runtime
            .try_occupy_live(
                "root".into(),
                Box::new(BarrierChild {
                    panic_on_release: false,
                    failure: failure.clone(),
                    turn: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                    entered: entered.clone(),
                    finish: finish.clone(),
                }),
            )
            .await
            .unwrap_or_else(|_| panic!("occupied"));
        let owner = runtime.clone();
        let caller = tokio::spawn(async move { owner.cancel_live("root").await });
        entered.notified().await;
        let completion = runtime.live.lock().await.retiring["root"]
            .completion
            .clone();
        tokio::time::advance(HOST_RETIRE_JOIN_TIMEOUT + Duration::from_secs(1)).await;
        assert!(caller.await.unwrap().is_err(), "joiner is bounded");
        assert!(
            runtime.live.lock().await.contains_key("root"),
            "release still owns slot"
        );
        finish.notify_one();
        let result = completion.await;
        assert!(
            recoveries.load(Ordering::SeqCst) >= 2,
            "both the deadline and the returned late release must trigger recovery"
        );
        assert_eq!(
            result.is_err(),
            failure.is_some(),
            "late release was abandoned: {result:?}"
        );
        assert!(!runtime.live.lock().await.contains_key("root"));
        runtime
            .read(|tree| {
                assert_eq!(tree.node("root").unwrap().session_locator.as_deref(), None);
                assert_eq!(tree.revision, tree.stored_revision);
            })
            .unwrap();
    }

    #[test]
    fn retirement_release_is_not_wrapped_in_timeout() {
        let source = include_str!("hub.rs");
        let worker = source
            .split("let task = tokio::spawn(async move {")
            .nth(1)
            .unwrap()
            .split("let retirement = Retirement {")
            .next()
            .unwrap();
        assert!(!worker.contains("tokio::time::timeout("));
    }

    #[tokio::test]
    async fn cancel_reservation_excludes_reoccupation_until_locator_publication() {
        use std::str::FromStr;
        let old = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        let new = TurnId::from_str("00000000-0000-0000-0000-000000000002").unwrap();
        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(ProductTopologyRuntime::new(
            bootstrap("retiring", root.path().to_path_buf()),
            memory_store(),
            Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
            None,
        ));
        let entered = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        let child = |turn| {
            Box::new(BarrierChild {
                panic_on_release: false,
                failure: None,
                turn,
                entered: entered.clone(),
                finish: finish.clone(),
            }) as Box<dyn LiveChildHandle>
        };
        assert!(
            runtime
                .try_occupy_live("root".into(), child(old))
                .await
                .is_ok()
        );
        let owner = runtime.clone();
        let cancel = tokio::spawn(async move { owner.cancel_live("root").await });
        entered.notified().await;
        assert!(
            runtime.live.try_lock().is_ok(),
            "release must not hold the map mutex"
        );
        assert!(
            runtime
                .try_occupy_live("root".into(), child(new))
                .await
                .is_err()
        );
        let mut overlapping = Box::pin(runtime.cancel_live("root"));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                overlapping.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        cancel.abort();
        assert!(cancel.await.unwrap_err().is_cancelled());
        finish.notify_one();
        assert_eq!(overlapping.await.unwrap(), LiveCancelOutcome::Cancelled);
        runtime
            .read(|tree| {
                assert_eq!(tree.node("root").unwrap().session_locator.as_deref(), None);
                assert_eq!(tree.revision, tree.stored_revision);
            })
            .unwrap();
        assert!(
            runtime
                .try_occupy_live("root".into(), child(new))
                .await
                .is_ok()
        );
        let mut live = runtime.live.lock().await;
        assert!(
            live.start_retire(
                runtime.live.clone(),
                runtime.driver.clone(),
                "root",
                Some(old),
                false
            )
            .is_none(),
            "old waiter cannot take replacement"
        );
        assert_eq!(live.get("root").unwrap().turn_id(), new);
    }
    #[tokio::test]
    async fn abandoned_caller_still_records_panicking_release_fault() {
        use std::str::FromStr;
        let root = tempfile::tempdir().unwrap();
        let store = memory_store();
        let runtime = Arc::new(ProductTopologyRuntime::new(
            bootstrap("panicking-retirement", root.path().to_path_buf()),
            store.clone(),
            Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
            None,
        ));
        runtime
            .mutate_then_emit(false, |tree| {
                tree.spawn_agent(
                    "root",
                    &crate::protocol::SpawnAgentRequest::test_review(),
                    None,
                )
                .unwrap();
                Ok(())
            })
            .unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        assert!(
            runtime
                .try_occupy_live(
                    "c1".into(),
                    Box::new(BarrierChild {
                        panic_on_release: true,
                        failure: None,
                        turn,
                        entered: entered.clone(),
                        finish: finish.clone(),
                    })
                )
                .await
                .is_ok()
        );
        let owner = runtime.clone();
        let caller = tokio::spawn(async move { owner.cancel_live("c1").await });
        entered.notified().await;
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        // Observe the owned completion without starting a second cancellation.
        let completion = runtime
            .live
            .lock()
            .await
            .retiring
            .get("c1")
            .unwrap()
            .completion
            .clone();
        finish.notify_one();
        let failure = completion.await.unwrap_err();
        assert_eq!(failure, "child retirement worker panicked");
        assert!(!runtime.live.lock().await.contains_key("c1"));
        runtime
            .read(|tree| {
                assert_eq!(
                    tree.node("c1").unwrap().status,
                    crate::protocol::NodeStatus::RecoveryRequired
                );
                assert_eq!(tree.fault_reason("c1"), Some(failure.as_str()));
            })
            .unwrap();
        let persisted = store.load("panicking-retirement").unwrap().unwrap();
        assert_eq!(
            persisted.rows().node("c1").unwrap().status,
            crate::protocol::NodeStatus::RecoveryRequired
        );
        assert_eq!(
            persisted
                .rows()
                .attempt("c1")
                .unwrap()
                .failure
                .as_ref()
                .unwrap()
                .message,
            failure
        );
        assert_eq!(
            runtime.cancel_live("c1").await.unwrap(),
            LiveCancelOutcome::NotLive
        );
    }

    #[tokio::test]
    async fn shutdown_joins_existing_retirement_and_concurrent_active_release() {
        use std::str::FromStr;
        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(ProductTopologyRuntime::new(
            bootstrap("shutdown-retiring", root.path().to_path_buf()),
            memory_store(),
            Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
            None,
        ));
        let child_id = runtime
            .mutate_then_emit(false, |tree| {
                Ok(tree
                    .spawn_agent(
                        "root",
                        &crate::protocol::SpawnAgentRequest::test_review(),
                        None,
                    )
                    .unwrap()
                    .agent_id)
            })
            .unwrap();
        let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        let second_entered = Arc::new(tokio::sync::Notify::new());
        let second_finish = Arc::new(tokio::sync::Notify::new());
        for (id, entered, finish) in [
            ("root".to_owned(), entered.clone(), finish.clone()),
            (
                child_id.clone(),
                second_entered.clone(),
                second_finish.clone(),
            ),
        ] {
            assert!(
                runtime
                    .try_occupy_live(
                        id,
                        Box::new(BarrierChild {
                            panic_on_release: false,
                            failure: None,
                            turn,
                            entered,
                            finish,
                        })
                    )
                    .await
                    .is_ok()
            );
        }
        let owner = runtime.clone();
        let cancel = tokio::spawn(async move { owner.cancel_live("root").await });
        entered.notified().await;
        let mut shutdown = Box::pin(shutdown_runtime(runtime.clone()));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                shutdown.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        second_entered.notified().await;
        second_finish.notify_one();
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                shutdown.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        assert!(runtime.live.try_lock().is_ok());
        finish.notify_one();
        shutdown.await.unwrap();
        assert_eq!(cancel.await.unwrap().unwrap(), LiveCancelOutcome::Cancelled);
        assert!(runtime.live.lock().await.is_empty());
        runtime
            .read(|tree| {
                for id in ["root", &child_id] {
                    assert_eq!(tree.node(id).unwrap().session_locator.as_deref(), None);
                }
                assert_eq!(tree.revision, tree.stored_revision);
            })
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_deadline_bounds_joiners_but_keeps_release_owner() {
        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(ProductTopologyRuntime::new(
            bootstrap("shutdown-stuck", root.path().to_path_buf()),
            memory_store(),
            Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
            None,
        ));
        let entered = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        assert!(
            runtime
                .try_occupy_live(
                    "root".into(),
                    Box::new(BarrierChild {
                        panic_on_release: false,
                        failure: None,
                        turn: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                        entered: entered.clone(),
                        finish: finish.clone(),
                    })
                )
                .await
                .is_ok()
        );
        let owner = runtime.clone();
        let cancel = tokio::spawn(async move { owner.cancel_live("root").await });
        entered.notified().await;
        let completion = runtime.live.lock().await.retiring["root"]
            .completion
            .clone();
        let result = shutdown_runtime(runtime.clone()).await.unwrap_err();
        assert_eq!(result.child_failures.len(), 1);
        assert!(runtime.live.lock().await.contains_key("root"));
        finish.notify_one();
        completion.await.expect("shutdown must not abandon release");
        cancel.await.unwrap().unwrap();
        assert!(!runtime.live.lock().await.contains_key("root"));
        runtime
            .read(|tree| {
                assert_eq!(tree.node("root").unwrap().session_locator.as_deref(), None);
            })
            .unwrap();
    }

    #[tokio::test]
    async fn refused_retirement_publication_clears_the_slot_but_keeps_fault() {
        use std::str::FromStr;
        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(ProductTopologyRuntime::new(
            bootstrap("retirement-publish-refused", root.path().to_path_buf()),
            Arc::new(RefusingStore),
            Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
            None,
        ));
        let entered = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        assert!(
            runtime
                .try_occupy_live(
                    "root".into(),
                    Box::new(BarrierChild {
                        panic_on_release: false,
                        failure: None,
                        turn: TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap(),
                        entered: entered.clone(),
                        finish: finish.clone(),
                    })
                )
                .await
                .is_ok()
        );
        let owner = runtime.clone();
        let cancel = tokio::spawn(async move { owner.cancel_live("root").await });
        entered.notified().await;
        finish.notify_one();
        let failure = cancel.await.unwrap().unwrap_err();
        assert_eq!(
            runtime.cancel_live("root").await.unwrap(),
            LiveCancelOutcome::NotLive
        );
        assert!(!runtime.live.lock().await.contains_key("root"));
        runtime
            .read(|tree| {
                assert_eq!(
                    tree.fault_reason("root"),
                    Some(failure.to_string().as_str())
                );
                assert_eq!(
                    tree.node("root").unwrap().status,
                    crate::protocol::NodeStatus::Running
                );
                assert_ne!(tree.revision, tree.stored_revision);
            })
            .unwrap();
    }
    #[tokio::test]
    async fn batch_cancel_starts_all_releases_before_waiting_and_joins_overlap() {
        use std::str::FromStr;
        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(ProductTopologyRuntime::new(
            bootstrap("batch-cancel", root.path().to_path_buf()),
            memory_store(),
            Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
            None,
        ));
        let child = runtime
            .mutate_then_emit(false, |tree| {
                Ok(tree
                    .spawn_agent(
                        "root",
                        &crate::protocol::SpawnAgentRequest::test_review(),
                        None,
                    )
                    .unwrap()
                    .agent_id)
            })
            .unwrap();
        let ids = vec![child, "root".to_owned()];
        let mut barriers = Vec::new();
        for id in &ids {
            let entered = Arc::new(tokio::sync::Notify::new());
            let finish = Arc::new(tokio::sync::Notify::new());
            assert!(
                runtime
                    .try_occupy_live(
                        id.clone(),
                        Box::new(BarrierChild {
                            panic_on_release: false,
                            failure: None,
                            turn: TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap(),
                            entered: entered.clone(),
                            finish: finish.clone(),
                        })
                    )
                    .await
                    .is_ok()
            );
            barriers.push((entered, finish));
        }
        let mut batch = Box::pin(runtime.cancel_live_many(&ids));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(batch.as_mut().poll(cx).is_pending()))
                .await
        );
        for (entered, _) in &barriers {
            tokio::time::timeout(Duration::from_secs(1), entered.notified())
                .await
                .unwrap();
        }
        let mut overlap = Box::pin(runtime.cancel_live(&ids[0]));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                overlap.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        barriers[1].1.notify_one();
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(batch.as_mut().poll(cx).is_pending()))
                .await
        );
        barriers[0].1.notify_one();
        let report = batch.await;
        assert!(report.is_clean());
        assert_eq!(report.cancelled, ids);
        assert_eq!(overlap.await.unwrap(), LiveCancelOutcome::Cancelled);
        assert!(runtime.live.lock().await.is_empty());
    }

    fn recovery_runtime(name: &str, root: &Path) -> Arc<ProductTopologyRuntime> {
        Arc::new(ProductTopologyRuntime::new(
            bootstrap(name, root.to_path_buf()),
            memory_store(),
            Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
            None,
        ))
    }

    /// A request while a pass runs is not dropped: the running pass is told to
    /// run once more, and only then gives up the slot.
    #[tokio::test]
    async fn recovery_pass_request_while_running_reruns_it() {
        let root = tempfile::tempdir().unwrap();
        let runtime = recovery_runtime("recovery-pass-rerun", root.path());
        let gate = Arc::new(tokio::sync::Notify::new());
        let runs = Arc::new(AtomicUsize::new(0));
        let (pass_gate, pass_runs, pass_runtime) = (gate.clone(), runs.clone(), runtime.clone());
        let started = runtime.start_recovery_pass(move || {
            tokio::spawn(async move {
                loop {
                    pass_gate.notified().await;
                    pass_runs.fetch_add(1, Ordering::SeqCst);
                    if !pass_runtime.finish_recovery_pass() {
                        break;
                    }
                }
            })
        });
        assert_eq!(started, RecoveryPassRequest::Started);
        let second = runtime.start_recovery_pass(|| unreachable!("a running pass reruns"));
        assert_eq!(second, RecoveryPassRequest::Rerun);
        gate.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while runs.load(Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            runtime.recovery_pass_running(),
            "the queued rerun holds the slot"
        );
        gate.notify_one();
        let task = runtime.recovery_pass.lock().unwrap().task.take().unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 2);
        assert!(!runtime.recovery_pass_running());
    }

    #[tokio::test]
    async fn recovery_pass_request_after_close_starts_nothing() {
        let root = tempfile::tempdir().unwrap();
        let runtime = recovery_runtime("recovery-pass-closed", root.path());
        runtime.close_for_shutdown();
        let request = runtime.start_recovery_pass(|| unreachable!("a closed hub starts nothing"));
        assert_eq!(request, RecoveryPassRequest::Closed);
        assert!(!runtime.recovery_pass_running());
    }

    /// A pass that never finishes gets its own bounded wait, then is aborted
    /// and joined briefly, so shutdown still ends.
    #[tokio::test]
    async fn shutdown_aborts_a_recovery_pass_that_overruns() {
        let root = tempfile::tempdir().unwrap();
        let runtime = recovery_runtime("recovery-pass-shutdown", root.path());
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel::<()>();
        let request = runtime.start_recovery_pass(move || {
            tokio::spawn(async move {
                let _dropped = dropped_tx;
                std::future::pending::<()>().await;
            })
        });
        assert_eq!(request, RecoveryPassRequest::Started);
        let begun = std::time::Instant::now();
        // The verdict on the tree's own teardown is not this test's subject.
        let _ = tokio::time::timeout(Duration::from_secs(4), shutdown_runtime(runtime.clone()))
            .await
            .expect("shutdown must not wait out a stuck recovery pass");
        assert!(begun.elapsed() < HOST_SHUTDOWN_RELEASE_TIMEOUT + Duration::from_millis(1500));
        assert!(
            dropped_rx.await.is_err(),
            "the aborted pass dropped its state"
        );
        assert!(runtime.is_closed());
    }
}
