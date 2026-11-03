use super::*;
use crate::hub::{LiveCancelOutcome, shutdown_runtime, test_support};
use crate::protocol::{AttemptScopedRequest, NodeStatus, SpawnAgentRequest};
use std::path::Path;

async fn fixture(root: &Path) -> (Arc<ProductTopologyRuntime>, String) {
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
        .await
        .unwrap();
    (runtime, id)
}

#[tokio::test]
async fn clean_startup_release_triggers_recovery_only_for_unresolved_nodes() {
    use std::sync::atomic::AtomicUsize;

    for unresolved in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (runtime, id) = fixture(root.path()).await;
        let startup = runtime.reserve_startup(&id).await.unwrap();
        if unresolved {
            runtime
                .mutate_then_emit(false, {
                    let id = id.clone();
                    move |tree| {
                        tree.record_host_retirement(&id, "caller deadline".into())
                            .and_then(|value| tree.fold_due(&id).map(|_| value))
                            .map_err(|error| error.message)
                    }
                })
                .await
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
    use crate::tree::{
        CustodyClaim, PersistedTree, ProductAgentTree, Revision, TreeCustody, TreeStore,
        TreeStoreError,
    };
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
        fn save(
            &self,
            tree: &ProductAgentTree,
            claim: &CustodyClaim,
        ) -> Result<Revision, TreeStoreError> {
            if self.refuse.load(Ordering::SeqCst) {
                Err(TreeStoreError::unavailable("injected publication failure"))
            } else {
                self.inner.save(tree, claim)
            }
        }

        fn custody_port(&self) -> &dyn TreeCustody {
            self.inner.custody_port()
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
        .await
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
    let (runtime, id) = fixture(root.path()).await;
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
    let (runtime, id) = fixture(root.path()).await;
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
    let (runtime, id) = fixture(root.path()).await;
    let startup = runtime.reserve_startup(&id).await.unwrap();
    assert!(runtime.reserve_startup(&id).await.is_err());
    runtime
        .mutate_then_emit(false, {
            let id = id.clone();
            move |tree| {
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
            }
        })
        .await
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
    let (runtime, id) = fixture(root.path()).await;
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
        .mutate_then_emit(false, {
            let id = id.clone();
            move |tree| {
                tree.record_host_materialization(&id)
                    .and_then(|value| tree.fold_due(&id).map(|_| value))
                    .unwrap();
                assert_eq!(tree.node(&id).unwrap().status, NodeStatus::RecoveryRequired);
                assert!(!tree.has_turn_owner(&id));
                assert!(!tree.node(&id).unwrap().active_turn);
                assert!(tree.attempt(&id).unwrap().candidate_id.is_none());
                assert_eq!(tree.node(&id).unwrap().session_locator.as_deref(), None);
                Ok(())
            }
        })
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn lost_start_ack_times_out_cancel_but_keeps_late_owner() {
    let root = tempfile::tempdir().unwrap();
    let (runtime, id) = fixture(root.path()).await;
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
    let (runtime, id) = fixture(root.path()).await;
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
    let (runtime, id) = fixture(root.path()).await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn definite_refusal_cleanup_excludes_late_cancellation() {
    let root = tempfile::tempdir().unwrap();
    let (runtime, id) = fixture(root.path()).await;
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
