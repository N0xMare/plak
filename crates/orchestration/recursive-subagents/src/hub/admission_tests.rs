//! Admission reads the installed state, never tree-mutex contention.

#![allow(clippy::unwrap_used, reason = "test-owned runtime and rendezvous")]

use super::*;
use crate::tree::TreeAdmissionState;
use std::sync::mpsc;

fn restored_tree(id: &str, root: &Path) -> ProductAgentTree {
    let mut tree = test_support::bootstrap(id, root.to_owned());
    tree.spawn_agent(
        "root",
        &crate::protocol::SpawnAgentRequest::test_review(),
        None,
    )
    .unwrap();
    tree.restore_paused();
    assert!(tree.quiescence_refusal().is_some());
    assert_eq!(tree.state, TreeAdmissionState::RestoredPaused);
    tree
}

struct ParkedCapacityStore {
    inner: crate::tree::MemoryTreeStore,
    full: AtomicBool,
}

impl TreeStore for ParkedCapacityStore {
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
        if self.full.load(Ordering::Acquire) {
            return Err(crate::tree::TreeStoreError::storage_full());
        }
        self.inner.save(tree)
    }
}

#[tokio::test]
async fn restored_nonquiescent_tree_is_paused_immediately_after_insert() {
    let root = tempfile::tempdir().unwrap();
    let id = test_support::unique_tree_id();
    let runtime = insert_root(id.clone(), restored_tree(&id, root.path()), None)
        .await
        .unwrap();
    assert_eq!(
        runtime.read(|tree| tree.state).unwrap(),
        TreeAdmissionState::RestoredPaused
    );
    assert_eq!(hub_tree_is_restored_paused(&id), Some(true));
    assert!(is_restored_paused(&id));
    force_release(&id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detached_capacity_refused_resume_publishes_live_then_rolls_back_paused() {
    let root = tempfile::tempdir().unwrap();
    let id = test_support::unique_tree_id();
    let store = Arc::new(ParkedCapacityStore {
        inner: crate::tree::MemoryTreeStore::new(),
        full: AtomicBool::new(false),
    });
    let runtime = insert_root_with(
        id.clone(),
        restored_tree(&id, root.path()),
        store.clone(),
        None,
    )
    .await
    .unwrap();
    let host = crate::tools::HostShutdown::new();
    runtime.driver().bind_host_shutdown(host.clone());
    assert_eq!(hub_tree_is_restored_paused(&id), Some(true));
    store.full.store(true, Ordering::Release);
    let writer = runtime.clone();
    let task = tokio::spawn(async move {
        let driver = writer.driver().clone();
        let folding = driver.clone();
        driver
            .write(move |tree| folding.fold_and_commit_guarded(tree, ProductAgentTree::resume_tree))
            .await
            .unwrap()
    });
    // The writer parks on the refused publication, holding a detached copy.
    tokio::time::timeout(Duration::from_secs(30), async {
        while !runtime.driver().storage_unavailable() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(runtime.driver().storage_unavailable());
    let state = runtime.read(|tree| tree.state).unwrap();
    assert_eq!(
        state,
        TreeAdmissionState::Live,
        "the guard released the resumed live image"
    );
    assert_eq!(
        hub_tree_is_restored_paused(&id),
        Some(state == TreeAdmissionState::RestoredPaused)
    );
    // The host's shutdown ends the park.
    host.signal();
    assert_eq!(
        task.await.unwrap().unwrap_err().code,
        crate::protocol::TopologyErrorCode::RecoveryStorageUnavailable
    );
    assert_eq!(
        runtime.read(|tree| tree.state).unwrap(),
        TreeAdmissionState::RestoredPaused
    );
    assert_eq!(hub_tree_is_restored_paused(&id), Some(true));
    store.full.store(false, Ordering::Release);
    force_release(&id).await;
}

fn with_tree_locked(runtime: &Arc<ProductTopologyRuntime>, check: impl FnOnce()) {
    std::thread::scope(|scope| {
        let (locked, ready) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let runtime = runtime.clone();
        let holder = scope.spawn(move || {
            runtime
                .read(|_| {
                    locked.send(()).unwrap();
                    let _ = released.recv();
                })
                .unwrap();
        });
        ready.recv_timeout(Duration::from_secs(30)).unwrap();
        check();
        drop(release);
        holder.join().unwrap();
    });
}

#[tokio::test]
async fn live_admission_readers_do_not_mistake_tree_contention_for_pause() {
    let root = tempfile::tempdir().unwrap();
    let id = test_support::unique_tree_id();
    let runtime = insert_root(
        id.clone(),
        test_support::bootstrap(&id, root.path().to_owned()),
        None,
    )
    .await
    .unwrap();
    with_tree_locked(&runtime, || {
        assert_eq!(hub_tree_is_restored_paused(&id), Some(false));
        assert!(!is_restored_paused(&id));
    });
    force_release(&id).await;
}

#[tokio::test]
async fn paused_admission_is_visible_under_contention_and_tracks_resume() {
    let root = tempfile::tempdir().unwrap();
    let id = test_support::unique_tree_id();
    let runtime = insert_root(
        id.clone(),
        test_support::bootstrap(&id, root.path().to_owned()),
        None,
    )
    .await
    .unwrap();
    // Startup legitimately lifts a singleton's restored pause. Install the
    // paused state after startup so this test exercises the attached reader.
    runtime
        .driver()
        .locked_for_test(|tree| tree.state = TreeAdmissionState::RestoredPaused);
    with_tree_locked(&runtime, || {
        assert_eq!(hub_tree_is_restored_paused(&id), Some(true));
        assert!(is_restored_paused(&id));
    });
    // Internal guarded changes and detached public mutations must both
    // publish the final state, including a transition back to a real pause.
    runtime
        .driver()
        .locked_for_test(|tree| tree.state = TreeAdmissionState::Live);
    with_tree_locked(&runtime, || {
        assert_eq!(hub_tree_is_restored_paused(&id), Some(false));
    });
    let ((), effects) = runtime
        .driver()
        .mutate(|tree| tree.state = TreeAdmissionState::RestoredPaused)
        .await
        .unwrap();
    assert!(effects.is_empty());
    runtime.driver().settle(effects).await.unwrap();
    with_tree_locked(&runtime, || {
        assert_eq!(hub_tree_is_restored_paused(&id), Some(true));
    });
    let ((), effects) = runtime
        .driver()
        .mutate(|tree| tree.state = TreeAdmissionState::Live)
        .await
        .unwrap();
    assert!(effects.is_empty());
    runtime.driver().settle(effects).await.unwrap();
    with_tree_locked(&runtime, || {
        assert_eq!(hub_tree_is_restored_paused(&id), Some(false));
    });
    force_release(&id).await;
}
