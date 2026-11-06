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
    /// so its share is released. A release the store refuses leaves the
    /// record naming this live process, which its next take joins and which
    /// ends with it.
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
        if let Err(error) = crate::tree::release_custody_hold(self.store.custody_port(), hold) {
            tracing::warn!(
                target: "plak.topology",
                error = %error,
                "agent tree custody was not released when its driver ended"
            );
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
