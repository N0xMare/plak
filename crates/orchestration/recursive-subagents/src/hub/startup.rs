//! Ownership reservations before a child has an acknowledged turn identity.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::FutureExt as _;
use plakous::model::TurnId;

use super::{
    HOST_RETIRE_JOIN_TIMEOUT, LiveChildHandle, LiveRelease, ProductAgentTree,
    ProductTopologyRuntime, publish_release, record_retirement_fault, retirement_needs_recovery,
};

/// Stage at which a child startup is waiting. A deadline is not cancellation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildStartupStage {
    /// Waiting to reserve the child slot under the hub admission lock.
    Admitting,
    /// Resolving the admitted binding and creating the session.
    Creating,
    /// Awaiting acknowledgement of the assignment command.
    Starting,
    /// Publishing the acknowledged turn and registering its live owner.
    Registering,
    /// Joining cancellation and releasing the session.
    Releasing,
}

/// Shared reservation held until startup transfers ownership or release settles.
#[derive(Clone)]
pub struct ChildStartup {
    ownership: Arc<AtomicU8>,
    stage: Arc<Mutex<ChildStartupStage>>,
    completion: tokio::sync::watch::Sender<Option<Result<(), String>>>,
    driver: crate::tools::TreeDriver,
    tree_id: String,
    agent_id: String,
    pub(super) expected_attempt: Option<crate::protocol::AttemptId>,
    transferred_turn: Arc<Mutex<Option<TurnId>>>,
}

impl ChildStartup {
    const ACTIVE: u8 = 0;
    const CANCELLED: u8 = 1;
    const CLEANUP: u8 = 2;

    fn new(driver: crate::tools::TreeDriver, agent_id: String) -> Self {
        let tree_id = driver.read(|tree| tree.tree_id.clone()).unwrap_or_default();
        let expected_attempt = driver
            .read(|tree| {
                tree.attempt(&agent_id)
                    .map(|attempt| attempt.attempt_id.clone())
            })
            .ok()
            .flatten();
        Self {
            ownership: Arc::new(AtomicU8::new(Self::ACTIVE)),
            stage: Arc::new(Mutex::new(ChildStartupStage::Creating)),
            completion: tokio::sync::watch::channel(None).0,
            driver,
            tree_id,
            agent_id,
            expected_attempt,
            transferred_turn: Arc::default(),
        }
    }

    /// Requests cancellation; ownership remains until the worker settles it.
    pub fn request_cancel(&self) {
        let _ = self.ownership.compare_exchange(
            Self::ACTIVE,
            Self::CANCELLED,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }

    /// Attempt captured by this reservation, never a later current attempt.
    #[must_use]
    pub fn attempt_id(&self) -> Option<&crate::protocol::AttemptId> {
        self.expected_attempt.as_ref()
    }

    /// Records only this startup's uncertainty, never a successor's fault.
    ///
    /// # Errors
    /// Returns the tree's intent-recording refusal.
    pub fn record_retirement_failure(
        &self,
        tree: &mut ProductAgentTree,
        message: String,
    ) -> Result<bool, crate::protocol::TopologyError> {
        let turn = *self
            .transferred_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if tree.tree_id != self.tree_id
            || turn.is_some_and(|expected| {
                tree.attempt(&self.agent_id)
                    .and_then(|attempt| attempt.turn_id.as_deref())
                    .is_some_and(|turn| turn != expected.to_string())
            })
        {
            return Ok(false);
        }
        tree.record_host_retirement_for_attempt(&self.agent_id, self.attempt_id(), message)
    }

    /// Whether the owner must release rather than start or register work.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.ownership.load(Ordering::SeqCst) == Self::CANCELLED
    }

    /// Claims exclusive authority to remove a newly created session after a
    /// definite pre-admission refusal. Once this succeeds, a late caller
    /// cancellation cannot race the release/archive/delete sequence.
    #[must_use]
    pub fn claim_definite_refusal_cleanup(&self) -> bool {
        self.ownership
            .compare_exchange(
                Self::ACTIVE,
                Self::CLEANUP,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// Records the current diagnostic stage without changing authority.
    pub fn set_stage(&self, stage: ChildStartupStage) {
        if let Ok(mut current) = self.stage.lock() {
            *current = stage;
        }
    }

    /// The stage that has not yet settled.
    #[must_use]
    pub fn stage(&self) -> ChildStartupStage {
        self.stage
            .lock()
            .map_or(ChildStartupStage::Releasing, |stage| *stage)
    }

    fn same_owner(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.ownership, &other.ownership)
    }

    pub(super) async fn join_cancel(
        &self,
    ) -> Result<super::LiveCancelOutcome, super::LiveCancelError> {
        self.request_cancel();
        // The startup owner's release has the same store-bounded deadline.
        if let Ok(result) = tokio::time::timeout(HOST_RETIRE_JOIN_TIMEOUT, self.join()).await {
            result
        } else {
            record_retirement_fault(
                &self.driver,
                &self.agent_id,
                self.expected_attempt.as_ref(),
                super::RetirementFaultSource::Joiner,
                &format!(
                    "child startup exceeded its join deadline at {:?}; owner retained",
                    self.stage()
                ),
            )
            .await;
            Err(super::LiveCancelError::JoinExpired)
        }
    }

    pub(super) async fn join(&self) -> Result<super::LiveCancelOutcome, super::LiveCancelError> {
        let mut completion = self.completion.subscribe();
        loop {
            if let Some(result) = completion.borrow_and_update().clone() {
                return result
                    .map(|()| super::LiveCancelOutcome::Cancelled)
                    .map_err(super::LiveCancelError::OwnerFailure);
            }
            completion.changed().await.map_err(|_| {
                super::LiveCancelError::OwnerFailure("child startup owner disappeared".to_owned())
            })?;
        }
    }
}

/// The refusal [`ProductTopologyRuntime::reserve_startup`] returns for a
/// child whose live slot another turn or startup holds: nothing was
/// reserved and nothing is at fault.
pub const STARTUP_SLOT_OCCUPIED: &str = "child is already running, starting, or retiring";

impl ProductTopologyRuntime {
    /// Cancels only this reservation or the exact turn it transferred. A late
    /// startup caller cannot retire a replacement child, even on the same attempt.
    pub async fn cancel_startup(&self, agent_id: &str, startup: &ChildStartup) {
        if agent_id != startup.agent_id
            || !self
                .driver
                .read(|tree| tree.tree_id == startup.tree_id)
                .unwrap_or(false)
        {
            return;
        }
        startup.request_cancel();
        let turn = {
            // Transfer checks cancellation under this same admission lock.
            // Taking it before reading prevents a cancelled caller from missing
            // a transfer that passed its check just before request_cancel.
            let _live = self.live.lock().await;
            *startup
                .transferred_turn
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        if let Some(turn) = turn {
            let _ = super::retire_live(&self.driver, &self.live, agent_id, Some(turn), true).await;
        }
    }

    /// Reserves a child before any session or turn operation can be admitted.
    ///
    /// # Errors
    /// Refuses closed hubs, occupied children ([`STARTUP_SLOT_OCCUPIED`]),
    /// and failed ownership publication.
    pub async fn reserve_startup(&self, agent_id: &str) -> Result<ChildStartup, String> {
        self.reserve_startup_admitted(agent_id, |_| Ok(())).await
    }

    /// [`Self::reserve_startup`], admitted only if `admit` accepts the tree
    /// under the hub admission lock, before the process-local reservation is
    /// installed. An interrupt's cancel needs that lock, so it either lands
    /// before the check (and `admit` sees it) or cancels the reservation.
    ///
    /// # Errors
    /// As [`Self::reserve_startup`], or the refusal `admit` returned.
    pub async fn reserve_startup_admitted(
        &self,
        agent_id: &str,
        admit: impl FnOnce(&ProductAgentTree) -> Result<(), String> + Send + 'static,
    ) -> Result<ChildStartup, String> {
        let mut live = self.live.lock().await;
        if !self.admits_live(&live, agent_id) {
            if self.is_closed() {
                return Err("topology hub is closed".into());
            }
            return Err(STARTUP_SLOT_OCCUPIED.into());
        }
        let agent = agent_id.to_owned();
        let reserved = agent_id.to_owned();
        self.driver
            .write_or_undo(
                move |tree| {
                    tree.detach();
                    let reserved = reserve(tree, admit, &agent);
                    // As a plain mutation: anything queued is not this
                    // reservation's to settle.
                    drop(tree.take_effects());
                    reserved
                },
                // A caller dropped before it holds the reservation releases it.
                move |tree, reserved_owner| {
                    if reserved_owner.is_ok() {
                        tree.release_turn_owner(&reserved);
                    }
                },
            )
            .await
            .map_err(|error| error.message)??;
        let startup = ChildStartup::new(self.driver.clone(), agent_id.to_owned());
        live.open(agent_id.to_owned(), startup.clone());
        Ok(startup)
    }
}

fn reserve(
    tree: &mut ProductAgentTree,
    admit: impl FnOnce(&ProductAgentTree) -> Result<(), String>,
    agent: &str,
) -> Result<(), String> {
    admit(tree)?;
    let node = tree.node(agent).map_err(|error| error.message)?;
    if node.status.is_terminal()
        || tree
            .attempt(agent)
            .is_some_and(|attempt| attempt.status.is_stopped())
    {
        return Err("terminal child cannot hold startup ownership".to_owned());
    }
    tree.reserve_turn_owner(agent);
    Ok(())
}

impl ProductTopologyRuntime {
    /// Atomically transfers a startup reservation to its acknowledged live turn.
    /// A cancelled reservation always returns the child to its startup owner.
    ///
    /// # Errors
    /// Returns the owned child if shutdown, cancellation, or another reservation
    /// prevents the transfer. The caller must cancel and release that child.
    pub async fn occupy_started_child(
        &self,
        agent_id: String,
        startup: &ChildStartup,
        child: Box<dyn LiveChildHandle>,
    ) -> Result<TurnId, Box<dyn LiveChildHandle>> {
        let mut live = self.live.lock().await;
        if self.is_closed()
            || startup.is_cancelled()
            || !live
                .opening(&agent_id)
                .is_some_and(|entry| entry.same_owner(startup))
        {
            return Err(child);
        }
        let turn = child.turn_id();
        let agent = agent_id.clone();
        if self
            .driver
            .mutate(move |tree| tree.bind_turn_owner(&agent, turn.to_string()))
            .await
            .is_err()
        {
            return Err(child);
        }
        *startup
            .transferred_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(turn);
        live.reside(agent_id, child);
        startup.completion.send_replace(Some(Ok(())));
        Ok(turn)
    }

    /// Publishes a returned release and frees its exact startup reservation.
    /// Failure retains durable recovery evidence, not a nonexistent live owner.
    pub async fn finish_startup(
        &self,
        agent_id: &str,
        startup: &ChildStartup,
        release: LiveRelease,
    ) {
        {
            let live = self.live.lock().await;
            if !live
                .opening(agent_id)
                .is_some_and(|entry| entry.same_owner(startup))
            {
                return;
            }
        }
        let result = std::panic::AssertUnwindSafe(publish_release(&self.driver, agent_id, release))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err("child startup release publication panicked".into()));
        if let Err(error) = &result {
            record_retirement_fault(
                &self.driver,
                agent_id,
                startup.expected_attempt.as_ref(),
                super::RetirementFaultSource::Owner,
                error,
            )
            .await;
        }
        let recovery = {
            let mut live = self.live.lock().await;
            if live
                .opening(agent_id)
                .is_some_and(|entry| entry.same_owner(startup))
            {
                live.vacate(agent_id);
                live.quiet.notify_one();
                let (writer, agent, released) =
                    (self.driver.clone(), agent_id.to_owned(), result.is_ok());
                let _ = self
                    .driver
                    .mutate(move |tree| {
                        tree.release_turn_owner(&agent);
                        if released
                            && tree.node(&agent).is_ok_and(|node| {
                                node.status == crate::protocol::NodeStatus::Closed
                            })
                        {
                            tree.try_emit_tree_node_detached(
                                writer.observer(),
                                &agent,
                                crate::tree::NodeDetach::Closed,
                            );
                        }
                    })
                    .await;
            }
            live.retirement_recovery.clone()
        };
        let needs_recovery = result.is_err() || retirement_needs_recovery(&self.driver, agent_id);
        startup.completion.send_replace(Some(result));
        if needs_recovery && let Some(recovery) = recovery {
            recovery();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests;
