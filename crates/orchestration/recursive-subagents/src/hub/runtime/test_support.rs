//! Tree mutations for test fixtures whose work borrows from their caller.

use super::{ProductAgentTree, ProductTopologyRuntime};

impl ProductTopologyRuntime {
    /// [`Self::mutate_then_emit`] for a test fixture whose work borrows from
    /// its caller: it runs on the calling task, in its writer turn, and a
    /// capacity-refused save inside it is refused rather than parked.
    ///
    /// # Errors
    ///
    /// As [`Self::mutate_then_emit`].
    pub async fn mutate_then_emit_borrowed_for_test<R>(
        &self,
        epoch_advanced: bool,
        work: impl FnOnce(&mut ProductAgentTree) -> Result<R, String>,
    ) -> Result<R, String> {
        let driver = self.driver.clone();
        let (result, effects) = self
            .driver
            .write_borrowed_for_test(|tree| {
                if tree.revision != tree.stored_revision {
                    driver.save_guarded(tree)?;
                }
                let result = driver.fold_and_commit_guarded(tree, |tree| {
                    work(tree).map_err(|message| {
                        crate::tree::TopologyError::new(
                            crate::protocol::TopologyErrorCode::OperationRecoveryRequired,
                            message,
                        )
                    })
                })?;
                if epoch_advanced && let Some(observer) = self.observer.as_ref() {
                    observer.epoch_advanced(&tree.tree_id);
                }
                Ok::<_, crate::tree::TopologyError>((result, tree.take_effects()))
            })
            .await
            .map_err(|error| error.message)?
            .map_err(|error| error.message)?;
        self.driver
            .settle(effects)
            .await
            .map_err(|error| error.message)?;
        Ok(result)
    }
}
