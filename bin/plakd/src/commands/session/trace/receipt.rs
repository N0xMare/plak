use plakous_hosted::agent_tree::{ChildArchiveReceipt, TreeStoreError};

use super::{
    AgentSessions, CliFailure, CliResult, PlakPaths, SessionLocator, SqliteTreeStore,
    TreeInspection,
};
use super::{selected_tree, settled_child};
use crate::command::TraceReceiptRecord;

pub(crate) fn record_receipt(paths: &PlakPaths, record: TraceReceiptRecord) -> CliResult<()> {
    let TraceReceiptRecord {
        id,
        attempt,
        head,
        revision,
        sha256,
        archive,
    } = record;
    let receipt = ChildArchiveReceipt {
        id: archive,
        sha256,
    };
    if !receipt.is_valid() {
        return Err(CliFailure::new(
            "expected a nonempty archive ID (at most 512 bytes, no control characters) and a lowercase hex SHA-256",
        ));
    }
    let (root, child) = super::super::parse_read_address(paths, &id)?;
    let child = child.ok_or_else(|| CliFailure::new("child session address required"))?;
    let TreeInspection::Tree(tree) = selected_tree(paths, &root)? else {
        return Err(CliFailure::new("unknown child: no persisted tree"));
    };
    let store = SqliteTreeStore::open(&paths.subagents_database())
        .map_err(|error| CliFailure::new(error.message()))?;
    store.record_child_archive_receipt(&tree.rows().tree_id, child, &receipt, |current| {
        let current = settled_child(current, child)
            .map_err(|error| TreeStoreError::unavailable(error.message()))?;
        let current_revision =
            AgentSessions::inspect_settled_revision_sqlite(paths.runtime_database(), current.locator)
                .map_err(|error| TreeStoreError::unavailable(format!("child receipt read failed: {error}")))?;
        if current.attempt.attempt_id.as_str() != attempt
            || head.parse::<SessionLocator>().ok() != Some(current.locator)
            || current_revision != revision
        {
            return Err(TreeStoreError::unavailable(
                "archive receipt does not match the current settled child attempt, head and revision",
            ));
        }
        Ok(revision)
    }).map_err(|error| CliFailure::new(error.detail))
}
