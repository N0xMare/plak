//! A tree driver's custody of its tree (spec 03 §14): taken when the driver
//! binds the tree to its store, presented by every save, given up when the
//! driver's last clone drops.

use std::sync::{Arc, Mutex};

use super::{ProductAgentTree, TreeDriver, TreeStore};

/// A driver's custody of its tree (spec 03 §14), taken when the driver bound
/// the tree to its store.
pub(super) struct DriverCustody {
    pub(super) store: Arc<dyn TreeStore>,
    /// The share this driver holds, or why its take was refused; every save
    /// is then refused the same way.
    pub(super) hold: Result<Mutex<Option<crate::tree::CustodyHold>>, crate::tree::CustodyError>,
}

impl DriverCustody {
    pub(super) fn claim(&self) -> Result<crate::tree::CustodyClaim, crate::tree::TreeStoreError> {
        match &self.hold {
            Ok(hold) => hold
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .map(|hold| hold.claim().clone())
                .ok_or_else(|| {
                    crate::tree::TreeStoreError::not_in_custody("agent tree custody was released")
                }),
            Err(error) => Err(error.clone().into()),
        }
    }
}

impl Drop for DriverCustody {
    /// The last clone of the driver is gone: no writer of this tree remains,
    /// so its share is released. Inside a Tokio runtime the blocking custody
    /// store release is offloaded via `spawn_blocking` so dropping a driver
    /// never stalls an async worker thread. If shutdown drops the queued
    /// release, the next process must prove the old holder gone before taking
    /// over the unreleased record.
    fn drop(&mut self) {
        let Ok(hold) = &self.hold else {
            return;
        };
        let Some(hold) = hold
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            return;
        };
        let store = Arc::clone(&self.store);
        let release = move || {
            if let Err(error) = crate::tree::release_custody_hold(store.custody_port(), hold) {
                tracing::warn!(
                    target: "plak.topology",
                    error = %error,
                    "agent tree custody was not released when its driver ended"
                );
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => drop(handle.spawn_blocking(release)),
            Err(_) => release(),
        }
    }
}

impl DriverCustody {
    /// Takes custody of `tree` in `store`, before the driver's first save.
    pub(super) fn take(tree: &Mutex<ProductAgentTree>, store: &Arc<dyn TreeStore>) -> Self {
        let tree_id = tree
            .lock()
            .map(|tree| tree.tree_id.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().tree_id.clone());
        Self {
            hold: crate::tree::acquire_custody(store.custody_port(), &tree_id)
                .map(|hold| Mutex::new(Some(hold))),
            store: Arc::clone(store),
        }
    }
}

impl DriverCustody {
    /// Gives this driver's share up now, for every clone of the driver:
    /// each later save is refused as not in custody, and the last clone's
    /// drop finds nothing to give up.
    fn release_now(&self) -> Result<(), crate::tree::CustodyError> {
        let Ok(hold) = &self.hold else {
            return Ok(());
        };
        let Some(hold) = hold
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            return Ok(());
        };
        crate::tree::release_custody_hold(self.store.custody_port(), hold)
    }
}

impl TreeDriver {
    /// Gives the tree's custody up now, at the end of a planned handoff's
    /// release (spec 03 §14), rather than when the driver's last clone
    /// drops: a child the release let go of while it still ran keeps a
    /// clone until its task ends, which would make the moment custody is
    /// given up depend on that task. The child's runtime branch lease
    /// still guards its session until then. Every later save of this
    /// driver is refused as not in custody. Giving up custody already given
    /// up does nothing.
    ///
    /// # Errors
    ///
    /// The store refused the release; the record still names this process,
    /// and ends with it.
    pub fn release_custody(&self) -> Result<(), crate::tree::CustodyError> {
        self.custody.release_now()
    }

    pub(super) fn save_claimed(
        &self,
        tree: &ProductAgentTree,
    ) -> Result<crate::tree::Revision, crate::tree::TreeStoreError> {
        let claim = self.custody.claim()?;
        self.store.save(tree, &claim)
    }

    /// Why this driver holds no custody of its tree, when it holds none.
    #[must_use]
    pub fn custody_refusal(&self) -> Option<crate::tree::TreeStoreError> {
        self.custody.claim().err()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{
        CustodyClaim, CustodyError, CustodyHost, CustodyRecord, HolderGone, HostInstance,
        MemoryTreeStore, PersistedTree, Revision, TreeCustody, TreeStoreError,
    };
    use std::collections::BTreeSet;
    use std::sync::mpsc;
    use std::time::Duration;

    struct BlockingReleaseStore {
        inner: MemoryTreeStore,
        entered: Mutex<Option<mpsc::Sender<std::thread::ThreadId>>>,
        unblock: Mutex<Option<mpsc::Receiver<()>>>,
        released: Mutex<Option<mpsc::Sender<()>>>,
    }

    impl TreeCustody for BlockingReleaseStore {
        fn custody_scope(&self) -> String {
            self.inner.custody_scope()
        }

        fn custody(&self, tree_id: &str) -> Result<Option<CustodyRecord>, TreeStoreError> {
            self.inner.custody(tree_id)
        }

        fn held_custody_instances(&self) -> Result<BTreeSet<HostInstance>, TreeStoreError> {
            self.inner.held_custody_instances()
        }

        fn take_custody(
            &self,
            tree_id: &str,
            host: &CustodyHost,
            takeover: Option<&HolderGone>,
        ) -> Result<CustodyClaim, CustodyError> {
            self.inner.take_custody(tree_id, host, takeover)
        }

        fn renew_custody(&self, claim: &CustodyClaim) -> Result<(), CustodyError> {
            self.inner.renew_custody(claim)
        }

        fn release_custody(&self, claim: &CustodyClaim) -> Result<(), CustodyError> {
            if let Some(tx) = self
                .entered
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                let _ = tx.send(std::thread::current().id());
            }
            if let Some(rx) = self
                .unblock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                let _ = rx.recv_timeout(Duration::from_secs(5));
            }
            let result = self.inner.release_custody(claim);
            if let Some(tx) = self
                .released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                let _ = tx.send(());
            }
            result
        }

        fn prove_custody(&self, claim: &CustodyClaim) -> Result<(), CustodyError> {
            self.inner.prove_custody(claim)
        }

        fn break_custody(&self, tree_id: &str) -> Result<Option<CustodyRecord>, CustodyError> {
            self.inner.break_custody(tree_id)
        }
    }

    impl TreeStore for BlockingReleaseStore {
        fn exists(&self, tree_id: &str) -> Result<bool, TreeStoreError> {
            self.inner.exists(tree_id)
        }

        fn ids(&self) -> Result<Vec<String>, TreeStoreError> {
            self.inner.ids()
        }

        fn save(
            &self,
            tree: &ProductAgentTree,
            claim: &CustodyClaim,
        ) -> Result<Revision, TreeStoreError> {
            self.inner.save(tree, claim)
        }

        fn load(&self, tree_id: &str) -> Result<Option<PersistedTree>, TreeStoreError> {
            self.inner.load(tree_id)
        }

        fn custody_port(&self) -> &dyn TreeCustody {
            self
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn driver_custody_drop_offloads_release_without_blocking_tokio_worker() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (unblock_tx, unblock_rx) = mpsc::channel();
        let (released_tx, released_rx) = mpsc::channel();
        let store: Arc<dyn TreeStore> = Arc::new(BlockingReleaseStore {
            inner: MemoryTreeStore::new(),
            entered: Mutex::new(Some(entered_tx)),
            unblock: Mutex::new(Some(unblock_rx)),
            released: Mutex::new(Some(released_tx)),
        });
        let tree_id = "driver-drop-offload-test";
        let hold =
            crate::tree::acquire_custody(store.custody_port(), tree_id).expect("acquire custody");
        let custody = DriverCustody {
            store: Arc::clone(&store),
            hold: Ok(Mutex::new(Some(hold))),
        };
        let worker_thread = std::thread::current().id();
        // If `Drop` ran `release_custody` inline, this single-threaded runtime
        // would deadlock waiting for `unblock_tx` below.
        drop(custody);

        let release_thread =
            tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(5)))
                .await
                .expect("entered waiter join")
                .expect("offloaded release entered");
        assert_ne!(
            release_thread, worker_thread,
            "DriverCustody::drop must offload blocking release off the Tokio worker"
        );
        let _ = unblock_tx.send(());
        tokio::task::spawn_blocking(move || released_rx.recv_timeout(Duration::from_secs(5)))
            .await
            .expect("released waiter join")
            .expect("offloaded release completed");
        let record = store
            .custody_port()
            .custody(tree_id)
            .expect("read custody")
            .expect("custody record");
        assert!(record.released);
    }
}
