//! Attaching a reopened tree: its reconciliation runs before anything else
//! reaches the driver.

use super::{ProductAgentTree, TopologyError, TreeDriver};

impl TreeDriver {
    /// Drains and completes the reopen-owned base-pin and refresh
    /// reconciliation through the port, outside the mutex and before any
    /// adopt retry: the hub runs it before it publishes the tree, so nothing
    /// else reaches this driver first. The reopened generation is published
    /// before any ref changes; a refused publication changes no pin and
    /// fails each reconciliation instead. Its results are recorded now and
    /// published by the startup fold or the first mutation.
    ///
    /// # Errors
    ///
    /// Refuses a poisoned tree.
    pub async fn reconcile_attachment(&self) -> Result<(), TopologyError> {
        // Most attachments owe nothing; they skip the writer altogether.
        if !self.read(ProductAgentTree::owes_attach_effects)? {
            return Ok(());
        }
        let driver = self.clone();
        let preparation = self
            .write_owed(move |tree| {
                let attach = tree.take_attach_effects();
                if attach.is_empty() {
                    return None;
                }
                let published = driver
                    .save_claimed(tree)
                    .map(|revision| tree.acknowledge_revision(revision));
                Some((attach, published))
            })
            .await?;
        let Some((attach, published)) = preparation else {
            return Ok(());
        };
        let completed = match published {
            Ok(()) => {
                plakous_workspace::occupancy_vcs::stage::unstaged(
                    "attach refresh and base pin, re-derived at the next attach",
                    attach.run(self.vcs.as_ref()),
                )
                .await
            }
            Err(error) => attach.refuse_attach(format!(
                "attachment publication failed: {error}; no base pin was changed"
            )),
        };
        self.write_owed(move |tree| {
            tree.complete(completed);
        })
        .await
    }
}
