//! A turn waiter registration's leave is owed.

#![allow(clippy::unwrap_used, reason = "registration leave fixtures")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::agent_tree::{
    CustodyClaim, MemoryTreeStore, PersistedTree, ProductAgentTree, Revision, TreeCustody,
    TreeStore, TreeStoreError,
};
use crate::agent_tree_protocol::{SpawnAgentRequest, WriterMode};

/// A tree store that refuses every save as full until told otherwise.
#[derive(Default)]
struct FullStore {
    inner: MemoryTreeStore,
    full: AtomicBool,
}

impl TreeStore for FullStore {
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
        if self.full.load(Ordering::SeqCst) {
            return Err(TreeStoreError::storage_full());
        }
        self.inner.save(tree, claim)
    }
    fn custody_port(&self) -> &dyn TreeCustody {
        self.inner.custody_port()
    }
}

/// A registration's leave queued behind a writer parked on a full store
/// still lands after its agent's storage waits are cancelled, once the
/// outage ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_leave_queued_during_an_outage_survives_its_agents_cancel() {
    let root = tempfile::tempdir().unwrap();
    let tree_id = uuid::Uuid::new_v4().to_string();
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        &tree_id,
        "root",
        "owner",
        root.path().to_owned(),
        "selected",
        WriterMode::SingleLease,
    )
    .unwrap();
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap()
        .agent_id;
    let store = Arc::new(FullStore::default());
    let runtime =
        crate::topology_runtime::insert_root_with(tree_id.clone(), tree, store.clone(), None)
            .await
            .unwrap();
    let driver = runtime.driver().clone();
    // The child's own writer, as a reopened child's waiter registers.
    let registration = super::TurnWaiterRegistration::register(&driver.for_agent(&child), &child)
        .await
        .unwrap();
    assert!(driver.read(|tree| tree.has_turn_waiter(&child)).unwrap());
    store.full.store(true, Ordering::SeqCst);
    let parked = driver.for_agent("root");
    let first = tokio::spawn(async move {
        let saver = parked.clone();
        parked.mutate(move |tree| saver.save(tree)).await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!first.is_finished(), "the writer parks on the full store");
    let leave = tokio::spawn(registration.release());
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    driver.cancel_storage_waits(&child);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!leave.is_finished(), "the leave waits for its turn");
    store.full.store(false, Ordering::SeqCst);
    let (saved, _effects) = first.await.unwrap().unwrap();
    saved.unwrap();
    // A liveness bound only: generous, so load never trips it.
    tokio::time::timeout(std::time::Duration::from_secs(30), leave)
        .await
        .expect("the leave lands after the outage")
        .unwrap();
    assert!(
        !driver.read(|tree| tree.has_turn_waiter(&child)).unwrap(),
        "the waiter count dropped"
    );
    crate::topology_runtime::force_release(&tree_id).await;
}
