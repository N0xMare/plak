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
    parked: Mutex<Option<mpsc::Sender<()>>>,
    release: Mutex<mpsc::Receiver<()>>,
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

impl ParkedCapacityStore {
    /// The park's shutdown probe: once full, it reports the park and holds it
    /// until the test releases it, then ends the wait.
    fn shutdown_probe(self: &Arc<Self>) -> Arc<dyn Fn() -> bool + Send + Sync> {
        let store = Arc::clone(self);
        Arc::new(move || {
            if !store.full.load(Ordering::Acquire) {
                return false;
            }
            if let Some(parked) = store.parked.lock().unwrap().take() {
                parked.send(()).unwrap();
                store.release.lock().unwrap().recv().unwrap();
            }
            true
        })
    }
}

struct ReleaseOnDrop(mpsc::Sender<()>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

#[tokio::test]
async fn restored_nonquiescent_tree_is_paused_immediately_after_insert() {
    let root = tempfile::tempdir().unwrap();
    let id = test_support::unique_tree_id();
    let runtime = insert_root(id.clone(), restored_tree(&id, root.path()), None).unwrap();
    assert_eq!(
        runtime.read(|tree| tree.state).unwrap(),
        TreeAdmissionState::RestoredPaused
    );
    assert_eq!(hub_tree_is_restored_paused(&id), Some(true));
    assert!(is_restored_paused(&id));
    force_release(&id).await;
}

#[tokio::test]
async fn detached_capacity_refused_resume_publishes_live_then_rolls_back_paused() {
    let root = tempfile::tempdir().unwrap();
    let id = test_support::unique_tree_id();
    let (parked, ready) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let store = Arc::new(ParkedCapacityStore {
        inner: crate::tree::MemoryTreeStore::new(),
        full: AtomicBool::new(false),
        parked: Mutex::new(Some(parked)),
        release: Mutex::new(released),
    });
    let runtime = insert_root_with(
        id.clone(),
        restored_tree(&id, root.path()),
        store.clone(),
        None,
    )
    .unwrap();
    runtime.driver().bind_storage_shutdown(store.shutdown_probe());
    assert_eq!(hub_tree_is_restored_paused(&id), Some(true));
    store.full.store(true, Ordering::Release);
    std::thread::scope(|scope| {
        let release = ReleaseOnDrop(release);
        let writer = runtime.clone();
        let task = scope.spawn(move || {
            let driver = writer.driver();
            let mut tree = driver.lock().unwrap();
            driver.fold_and_commit_guarded(&mut tree, ProductAgentTree::resume_tree)
        });
        ready.recv_timeout(Duration::from_secs(30)).unwrap();
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
        drop(release);
        assert_eq!(
            task.join().unwrap().unwrap_err().code,
            crate::protocol::TopologyErrorCode::RecoveryStorageUnavailable
        );
    });
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
    .unwrap();
    // Startup legitimately lifts a singleton's restored pause. Install the
    // paused state after startup so this test exercises the attached reader.
    runtime.driver().lock().unwrap().state = TreeAdmissionState::RestoredPaused;
    with_tree_locked(&runtime, || {
        assert_eq!(hub_tree_is_restored_paused(&id), Some(true));
        assert!(is_restored_paused(&id));
    });
    // Internal guarded changes and detached public mutations must both
    // publish the final state, including a transition back to a real pause.
    runtime.driver().lock().unwrap().state = TreeAdmissionState::Live;
    with_tree_locked(&runtime, || {
        assert_eq!(hub_tree_is_restored_paused(&id), Some(false));
    });
    let ((), effects) = runtime
        .driver()
        .mutate(|tree| tree.state = TreeAdmissionState::RestoredPaused)
        .unwrap();
    assert!(effects.is_empty());
    runtime.driver().settle(effects).unwrap();
    with_tree_locked(&runtime, || {
        assert_eq!(hub_tree_is_restored_paused(&id), Some(true));
    });
    let ((), effects) = runtime
        .driver()
        .mutate(|tree| tree.state = TreeAdmissionState::Live)
        .unwrap();
    assert!(effects.is_empty());
    runtime.driver().settle(effects).unwrap();
    with_tree_locked(&runtime, || {
        assert_eq!(hub_tree_is_restored_paused(&id), Some(false));
    });
    force_release(&id).await;
}
