//! Tree writes for test fixtures whose work borrows from their caller.

use super::{Effects, ProductAgentTree, TopologyError, TreeDriver, storage};

impl TreeDriver {
    /// [`Self::write`] for a test fixture whose work borrows from its
    /// caller: it runs on the calling task, in its writer turn, and a
    /// capacity-refused save inside it is refused rather than parked.
    ///
    /// # Errors
    ///
    /// Refuses a poisoned tree.
    pub(crate) async fn write_borrowed_for_test<R>(
        &self,
        work: impl FnOnce(&mut storage::TreeMutationGuard<'_>) -> R,
    ) -> Result<R, TopologyError> {
        let order = self.writer.order();
        let _permit = order.lock().await;
        let mut tree =
            storage::TreeMutationGuard::lock(&self.tree).map_err(|()| Self::lock_poisoned())?;
        Ok(work(&mut tree))
    }

    /// [`Self::mutate`] for a test fixture whose work borrows from its
    /// caller: it runs on the calling task, in its writer turn, and a
    /// capacity-refused save inside it is refused rather than parked.
    ///
    /// # Errors
    ///
    /// Refuses a poisoned tree.
    pub async fn mutate_borrowed_for_test<R>(
        &self,
        work: impl FnOnce(&mut ProductAgentTree) -> R,
    ) -> Result<(R, Effects), TopologyError> {
        self.write_borrowed_for_test(|tree| {
            tree.detach();
            let result = work(tree);
            (result, tree.take_effects())
        })
        .await
    }
}

impl TreeDriver {
    /// A test's stand-in for the next host's custody take after a planned
    /// handoff gave the tree's custody up ([`Self::release_custody`]): this
    /// driver takes it again.
    ///
    /// # Errors
    ///
    /// The take was refused.
    pub fn retake_custody_for_test(&self) -> Result<(), crate::tree::CustodyError> {
        let Ok(slot) = &self.custody.hold else {
            return Ok(());
        };
        let tree_id = self
            .tree
            .lock()
            .map(|tree| tree.tree_id.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().tree_id.clone());
        let mut slot = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(crate::tree::acquire_custody(
                self.custody.store.custody_port(),
                &tree_id,
            )?);
        }
        Ok(())
    }
}
