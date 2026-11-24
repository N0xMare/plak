use plakous_recursive_subagents::protocol::AttemptId;
use plakous_recursive_subagents::tree::{ChildArchiveReceipt, ProductAgentTree, TreeStoreError};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use super::{SqliteTreeStore, commit, load_agent_tree, sqlite_unavailable};

pub(super) fn read(
    db: &Connection,
    tree_id: &str,
    child: &str,
    attempt: &AttemptId,
    head: &str,
    revision: u64,
) -> Result<Option<ChildArchiveReceipt>, TreeStoreError> {
    let revision = crate::to_i64(revision).map_err(TreeStoreError::corrupt)?;
    let receipt = db
        .query_row(
            "SELECT receipt_id,sha256 FROM agent_child_archive_receipts
             WHERE tree_id=?1 AND agent_id=?2 AND attempt_id=?3 AND head=?4 AND runtime_revision=?5",
            (tree_id, child, attempt.as_str(), head, revision),
            |row| Ok(ChildArchiveReceipt { id: row.get(0)?, sha256: row.get(1)? }),
        )
        .optional()
        .map_err(|error| sqlite_unavailable("could not read child archive receipt", &error))?;
    Ok(receipt.filter(ChildArchiveReceipt::is_valid))
}

impl SqliteTreeStore {
    /// Inserts an immutable child archive attestation without changing the tree image or custody.
    /// The caller validates the exact settled trace identity and supplies its runtime revision.
    /// The tree snapshot remains fenced against writes throughout validation and publication.
    ///
    /// # Errors
    ///
    /// Refuses invalid attestations, a missing child, conflicts, validation failures and write errors.
    pub fn record_child_archive_receipt(
        &self,
        tree_id: &str,
        child: &str,
        receipt: &ChildArchiveReceipt,
        validate: impl FnOnce(&ProductAgentTree) -> Result<u64, TreeStoreError>,
    ) -> Result<(), TreeStoreError> {
        if !receipt.is_valid() {
            return Err(TreeStoreError::unavailable("invalid child archive receipt"));
        }
        self.with_connection(|db| {
            let tx = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|error| {
                    sqlite_unavailable("could not begin child archive receipt write", &error)
                })?;
            let tree = load_agent_tree(&tx, tree_id)
                .map_err(TreeStoreError::corrupt)?
                .ok_or_else(|| TreeStoreError::unavailable("child tree no longer exists"))?;
            let revision = validate(tree.rows())?;
            let node = tree
                .rows()
                .nodes()
                .get(child)
                .filter(|node| node.parent_agent_id.is_some())
                .ok_or_else(|| TreeStoreError::unavailable("child no longer exists"))?;
            let (Some(attempt), Some(head)) = (&node.attempt_id, &node.session_locator) else {
                return Err(TreeStoreError::unavailable(
                    "child has no exact trace identity",
                ));
            };
            let stored_revision = crate::to_i64(revision).map_err(TreeStoreError::corrupt)?;
            let inserted = tx
                .execute(
                    "INSERT INTO agent_child_archive_receipts
                 (tree_id,agent_id,attempt_id,head,runtime_revision,receipt_id,sha256)
                 VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT DO NOTHING",
                    (
                        tree_id,
                        child,
                        attempt.as_str(),
                        head,
                        stored_revision,
                        &receipt.id,
                        &receipt.sha256,
                    ),
                )
                .map_err(|error| {
                    sqlite_unavailable("could not insert child archive receipt", &error)
                })?;
            if inserted == 0
                && read(&tx, tree_id, child, attempt, head, revision)?.as_ref() != Some(receipt)
            {
                return Err(TreeStoreError::unavailable(
                    "conflicting archive receipt for this child revision",
                ));
            }
            commit(tx)
        })
    }
}
