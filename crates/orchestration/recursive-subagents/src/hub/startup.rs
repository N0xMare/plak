//! Ownership reservations before a child has an acknowledged turn identity.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use plakous::model::TurnId;

use super::{
    HOST_RETIRE_JOIN_TIMEOUT, LiveChildHandle, LiveRelease, ProductTopologyRuntime,
    publish_release, record_retirement_fault, retirement_needs_recovery,
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
    agent_id: String,
    pub(super) expected_attempt: Option<crate::protocol::AttemptId>,
}

impl ChildStartup {
    const ACTIVE: u8 = 0;
    const CANCELLED: u8 = 1;
    const CLEANUP: u8 = 2;

    fn new(driver: crate::tools::TreeDriver, agent_id: String) -> Self {
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
            agent_id,
            expected_attempt,
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
        let mut completion = self.completion.subscribe();
        let joined = async {
            loop {
                if let Some(result) = completion.borrow_and_update().clone() {
                    return result
                        .map(|()| super::LiveCancelOutcome::Cancelled)
                        .map_err(super::LiveCancelError::OwnerFailure);
                }
                completion.changed().await.map_err(|_| {
                    super::LiveCancelError::OwnerFailure(
                        "child startup owner disappeared".to_owned(),
                    )
                })?;
            }
        };
        // The startup owner's release is the same cancel-and-release a
        // retirement runs, so it shares the retirement's store-bounded deadline.
        if let Ok(result) = tokio::time::timeout(HOST_RETIRE_JOIN_TIMEOUT, joined).await {
            result
        } else {
            // The reservation owns startup deadline evidence, not its callers.
            record_retirement_fault(
                &self.driver,
                &self.agent_id,
                self.expected_attempt.as_ref(),
                super::RetirementFaultSource::Joiner,
                &format!(
                    "child startup exceeded its join deadline at {:?}; owner retained",
                    self.stage()
                ),
            );
            Err(super::LiveCancelError::JoinExpired)
        }
    }
}

/// The refusal [`ProductTopologyRuntime::reserve_startup`] returns for a
/// child whose live slot another turn or startup holds: nothing was
/// reserved and nothing is at fault.
pub const STARTUP_SLOT_OCCUPIED: &str = "child is already running, starting, or retiring";

impl ProductTopologyRuntime {
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
        admit: impl FnOnce(&crate::tree::ProductAgentTree) -> Result<(), String>,
    ) -> Result<ChildStartup, String> {
        let mut live = self.live.lock().await;
        if !self.admits_live(&live, agent_id) {
            if self.is_closed() {
                return Err("topology hub is closed".into());
            }
            return Err(STARTUP_SLOT_OCCUPIED.into());
        }
        self.driver
            .mutate(|tree| {
                admit(tree)?;
                let node = tree.node(agent_id).map_err(|error| error.message)?;
                if node.status.is_terminal()
                    || tree
                        .attempt(agent_id)
                        .is_some_and(|attempt| attempt.status.is_stopped())
                {
                    return Err("terminal child cannot hold startup ownership".to_owned());
                }
                tree.reserve_turn_owner(agent_id);
                Ok::<_, String>(())
            })
            .map_err(|error| error.message)?
            .0?;
        let startup = ChildStartup::new(self.driver.clone(), agent_id.to_owned());
        live.starting.insert(agent_id.to_owned(), startup.clone());
        Ok(startup)
    }

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
                .starting
                .get(&agent_id)
                .is_some_and(|entry| entry.same_owner(startup))
        {
            return Err(child);
        }
        let turn = child.turn_id();
        if self
            .driver
            .mutate(|tree| tree.bind_turn_owner(&agent_id, turn.to_string()))
            .is_err()
        {
            return Err(child);
        }
        live.starting.remove(&agent_id);
        live.children.insert(agent_id, child);
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
                .starting
                .get(agent_id)
                .is_some_and(|entry| entry.same_owner(startup))
            {
                return;
            }
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            publish_release(&self.driver, agent_id, release)
        }))
        .unwrap_or_else(|_| Err("child startup release publication panicked".into()));
        if let Err(error) = &result {
            record_retirement_fault(
                &self.driver,
                agent_id,
                startup.expected_attempt.as_ref(),
                super::RetirementFaultSource::Owner,
                error,
            );
        }
        let recovery = {
            let mut live = self.live.lock().await;
            if live
                .starting
                .get(agent_id)
                .is_some_and(|entry| entry.same_owner(startup))
            {
                live.starting.remove(agent_id);
                let _ = self.driver.mutate(|tree| tree.release_turn_owner(agent_id));
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
mod tests {
    use super::*;
    use crate::hub::{LiveCancelOutcome, shutdown_runtime, test_support};
    use crate::protocol::{AttemptScopedRequest, NodeStatus, SpawnAgentRequest};
    use std::path::Path;

    fn fixture(root: &Path) -> (Arc<ProductTopologyRuntime>, String) {
        let runtime = Arc::new(ProductTopologyRuntime::new(
            test_support::bootstrap("startup-test", root.to_path_buf()),
            test_support::memory_store(),
            Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
            None,
        ));
        let id = runtime
            .mutate_then_emit(false, |tree| {
                Ok(tree
                    .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                    .unwrap()
                    .agent_id)
            })
            .unwrap();
        (runtime, id)
    }

    #[tokio::test]
    async fn clean_startup_release_triggers_recovery_only_for_unresolved_nodes() {
        use std::sync::atomic::AtomicUsize;

        for unresolved in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let (runtime, id) = fixture(root.path());
            let startup = runtime.reserve_startup(&id).await.unwrap();
            if unresolved {
                runtime
                    .mutate_then_emit(false, |tree| {
                        tree.record_host_retirement(&id, "caller deadline".into())
                            .and_then(|value| tree.fold_due(&id).map(|_| value))
                            .map_err(|error| error.message)
                    })
                    .unwrap();
            }
            let calls = Arc::new(AtomicUsize::new(0));
            let triggered = calls.clone();
            runtime
                .set_retirement_recovery(Arc::new(move || {
                    triggered.fetch_add(1, Ordering::SeqCst);
                }))
                .await;
            runtime
                .finish_startup(&id, &startup, LiveRelease::default())
                .await;
            assert_eq!(calls.load(Ordering::SeqCst), usize::from(unresolved));
        }
    }

    #[tokio::test]
    async fn startup_publication_failure_clears_reservation_but_keeps_fault() {
        use crate::tree::{PersistedTree, ProductAgentTree, Revision, TreeStore, TreeStoreError};
        use std::sync::atomic::AtomicBool;

        struct Store {
            inner: Arc<dyn TreeStore>,
            refuse: AtomicBool,
        }
        impl TreeStore for Store {
            fn exists(&self, id: &str) -> Result<bool, TreeStoreError> {
                self.inner.exists(id)
            }
            fn ids(&self) -> Result<Vec<String>, TreeStoreError> {
                self.inner.ids()
            }
            fn load(&self, id: &str) -> Result<Option<PersistedTree>, TreeStoreError> {
                self.inner.load(id)
            }
            fn save(&self, tree: &ProductAgentTree) -> Result<Revision, TreeStoreError> {
                if self.refuse.load(Ordering::SeqCst) {
                    Err(TreeStoreError::unavailable("injected publication failure"))
                } else {
                    self.inner.save(tree)
                }
            }
        }
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store {
            inner: test_support::memory_store(),
            refuse: AtomicBool::new(false),
        });
        let runtime = Arc::new(ProductTopologyRuntime::new(
            test_support::bootstrap("startup-publish-test", root.path().to_path_buf()),
            store.clone(),
            Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
            None,
        ));
        let id = runtime
            .mutate_then_emit(false, |tree| {
                Ok(tree
                    .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                    .unwrap()
                    .agent_id)
            })
            .unwrap();
        let startup = runtime.reserve_startup(&id).await.unwrap();
        store.refuse.store(true, Ordering::SeqCst);
        runtime
            .finish_startup(
                &id,
                &startup,
                LiveRelease {
                    feed: None,
                    locator: Some("returned-head".into()),
                    failure: None,
                },
            )
            .await;
        assert!(!runtime.live.lock().await.contains_key(&id));
        assert!(startup.join_cancel().await.is_err());
        runtime
            .read(|tree| {
                assert_eq!(tree.node(&id).unwrap().status, NodeStatus::Running);
                assert!(tree.fault_reason(&id).is_some());
                assert_ne!(tree.revision, tree.stored_revision);
            })
            .unwrap();
    }

    #[tokio::test]
    async fn startup_ownership_never_writes_the_active_turn_view() {
        let root = tempfile::tempdir().unwrap();
        let (runtime, id) = fixture(root.path());
        let before = runtime.read(|tree| tree.revision).unwrap();
        let startup = runtime.reserve_startup(&id).await.unwrap();
        runtime
            .read(|tree| {
                assert!(!tree.node(&id).unwrap().active_turn);
                assert_eq!(tree.attempt(&id).unwrap().turn_id, None);
                assert_eq!(tree.revision, before);
            })
            .unwrap();
        runtime
            .finish_startup(&id, &startup, LiveRelease::default())
            .await;
    }

    #[tokio::test]
    async fn definite_precreate_refusal_releases_empty_reservation() {
        let root = tempfile::tempdir().unwrap();
        let (runtime, id) = fixture(root.path());
        let startup = runtime.reserve_startup(&id).await.unwrap();
        // No session create or command was admitted: no release remains to join.
        runtime
            .finish_startup(&id, &startup, LiveRelease::default())
            .await;
        assert_eq!(
            runtime.cancel_live(&id).await.unwrap(),
            LiveCancelOutcome::NotLive
        );
        assert!(!runtime.live.lock().await.contains_key(&id));
        runtime
            .read(|tree| {
                assert!(!tree.node(&id).unwrap().active_turn);
                assert!(tree.fault_reason(&id).is_none());
                assert_eq!(tree.revision, tree.stored_revision);
            })
            .unwrap();
    }

    #[tokio::test]
    async fn pending_startup_reserves_close_until_late_release() {
        let root = tempfile::tempdir().unwrap();
        let (runtime, id) = fixture(root.path());
        let startup = runtime.reserve_startup(&id).await.unwrap();
        assert!(runtime.reserve_startup(&id).await.is_err());
        runtime
            .mutate_then_emit(false, |tree| {
                tree.close_agent(
                    "root",
                    &AttemptScopedRequest {
                        agent_id: id.clone(),
                        expected_attempt_id: None,
                    },
                    None,
                )
                .unwrap();
                assert!(tree.has_turn_owner(&id));
                assert!(!tree.node(&id).unwrap().active_turn);
                assert!(tree.attempt(&id).unwrap().candidate_id.is_none());
                Ok(())
            })
            .unwrap();
        let owner = runtime.clone();
        let child = id.clone();
        let cancel = tokio::spawn(async move { owner.cancel_live(&child).await });
        while !startup.is_cancelled() {
            tokio::task::yield_now().await;
        }
        assert!(!cancel.is_finished());
        assert!(runtime.live.try_lock().is_ok());
        runtime
            .finish_startup(
                &id,
                &startup,
                LiveRelease {
                    feed: None,
                    locator: Some("late-released-head".into()),
                    failure: None,
                },
            )
            .await;
        assert_eq!(cancel.await.unwrap().unwrap(), LiveCancelOutcome::Cancelled);
        assert!(!runtime.live.lock().await.contains_key(&id));
        runtime
            .read(|tree| {
                assert!(!tree.node(&id).unwrap().active_turn);
                assert_eq!(tree.node(&id).unwrap().session_locator.as_deref(), None);
                assert_eq!(tree.revision, tree.stored_revision);
            })
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn startup_release_failure_clears_reservation_but_keeps_fault_and_no_seal() {
        let root = tempfile::tempdir().unwrap();
        let (runtime, id) = fixture(root.path());
        let startup = runtime.reserve_startup(&id).await.unwrap();
        startup.set_stage(ChildStartupStage::Releasing);
        assert!(runtime.cancel_live(&id).await.is_err());
        assert!(runtime.live.lock().await.contains_key(&id));
        runtime
            .finish_startup(
                &id,
                &startup,
                LiveRelease {
                    feed: None,
                    locator: Some("late-failed-release-head".into()),
                    failure: Some("injected release failure".into()),
                },
            )
            .await;
        assert_eq!(
            runtime.cancel_live(&id).await.unwrap(),
            LiveCancelOutcome::NotLive
        );
        assert!(!runtime.live.lock().await.contains_key(&id));
        runtime
            .mutate_then_emit(false, |tree| {
                tree.record_host_materialization(&id)
                    .and_then(|value| tree.fold_due(&id).map(|_| value))
                    .unwrap();
                assert_eq!(tree.node(&id).unwrap().status, NodeStatus::RecoveryRequired);
                assert!(!tree.has_turn_owner(&id));
                assert!(!tree.node(&id).unwrap().active_turn);
                assert!(tree.attempt(&id).unwrap().candidate_id.is_none());
                assert_eq!(tree.node(&id).unwrap().session_locator.as_deref(), None);
                Ok(())
            })
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn lost_start_ack_times_out_cancel_but_keeps_late_owner() {
        let root = tempfile::tempdir().unwrap();
        let (runtime, id) = fixture(root.path());
        let startup = runtime.reserve_startup(&id).await.unwrap();
        startup.set_stage(ChildStartupStage::Starting);
        let error = runtime.cancel_live(&id).await.unwrap_err();
        assert_eq!(error, super::super::LiveCancelError::JoinExpired);
        assert!(runtime.live.lock().await.contains_key(&id));
        runtime
            .read(|tree| {
                assert_eq!(tree.node(&id).unwrap().status, NodeStatus::RecoveryRequired);
                assert!(tree.has_turn_owner(&id));
                assert!(!tree.node(&id).unwrap().active_turn);
                assert_eq!(tree.revision, tree.stored_revision);
            })
            .unwrap();
        runtime
            .finish_startup(
                &id,
                &startup,
                LiveRelease {
                    feed: None,
                    locator: Some("late-head".into()),
                    failure: None,
                },
            )
            .await;
        assert!(!runtime.live.lock().await.contains_key(&id));
        assert_eq!(
            runtime.cancel_live(&id).await.unwrap(),
            LiveCancelOutcome::NotLive
        );
    }

    /// A startup release runs the same cancel-and-release as a retirement, so
    /// a slow but healthy one within the retirement deadline is not fenced.
    #[tokio::test(start_paused = true)]
    async fn slow_startup_release_within_the_retire_deadline_is_not_fenced() {
        let root = tempfile::tempdir().unwrap();
        let (runtime, id) = fixture(root.path());
        let startup = runtime.reserve_startup(&id).await.unwrap();
        startup.set_stage(ChildStartupStage::Releasing);
        let owner = runtime.clone();
        let child = id.clone();
        let mut caller = tokio::spawn(async move { owner.cancel_live(&child).await });
        while !startup.is_cancelled() {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(
            plakous_host_protocol::limits::HOST_CANCEL_SETTLE_TIMEOUT
                + crate::hub::HOST_SHUTDOWN_RELEASE_TIMEOUT
                + std::time::Duration::from_secs(13),
        )
        .await;
        tokio::task::yield_now().await;
        assert!(
            tokio::time::timeout(std::time::Duration::ZERO, &mut caller)
                .await
                .is_err(),
            "cancel waits for a startup release still within its bound"
        );
        runtime
            .read(|tree| assert_eq!(tree.fault_reason(&id), None))
            .unwrap();
        runtime
            .finish_startup(&id, &startup, LiveRelease::default())
            .await;
        assert_eq!(caller.await.unwrap(), Ok(LiveCancelOutcome::Cancelled));
        runtime
            .read(|tree| assert_eq!(tree.fault_reason(&id), None))
            .unwrap();
    }

    #[tokio::test]
    async fn shutdown_requests_startup_cancellation_and_joins_release() {
        let root = tempfile::tempdir().unwrap();
        let (runtime, id) = fixture(root.path());
        let startup = runtime.reserve_startup(&id).await.unwrap();
        let owner = runtime.clone();
        let shutdown = tokio::spawn(async move { shutdown_runtime(owner).await });
        while !startup.is_cancelled() {
            tokio::task::yield_now().await;
        }
        assert!(runtime.reserve_startup("root").await.is_err());
        assert!(!shutdown.is_finished());
        runtime
            .finish_startup(&id, &startup, LiveRelease::default())
            .await;
        shutdown.await.unwrap().unwrap();
        assert!(runtime.live.lock().await.is_empty());
    }

    #[test]
    fn definite_refusal_cleanup_excludes_late_cancellation() {
        let root = tempfile::tempdir().unwrap();
        let (runtime, id) = fixture(root.path());
        let startup = ChildStartup::new(runtime.driver.clone(), id.clone());
        assert!(startup.claim_definite_refusal_cleanup());
        startup.request_cancel();
        assert!(
            !startup.is_cancelled(),
            "cleanup ownership must remain stable across later cancellation requests"
        );
        assert!(!startup.claim_definite_refusal_cleanup());

        let cancelled = ChildStartup::new(runtime.driver.clone(), id);
        cancelled.request_cancel();
        assert!(cancelled.is_cancelled());
        assert!(
            !cancelled.claim_definite_refusal_cleanup(),
            "a cancellation that won first retains the fresh session for later GC"
        );
    }
}
