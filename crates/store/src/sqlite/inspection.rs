//! Read-only native checkpoint inspection; never owns a runtime or lease.
use super::{
    BranchHead, CheckpointId, Connection, LoadedBranch, RuntimeCheckpointInspection,
    RuntimeCheckpointKind, SessionRef, SessionStoreError, SqliteSessionStore, StoredCheckpoint,
    admit_inspection_snapshot, branch_row, branch_row_with_checkpoint, bundle_error, live_session,
    load_branch, load_checkpoint, load_object, map_sqlite,
};
use std::path::Path;

impl SqliteSessionStore {
    /// Reads one immutable checkpoint without opening a writer or changing leases.
    ///
    /// # Errors
    /// Returns a typed lookup, shape, integrity, or storage failure.
    pub fn inspect_checkpoint(
        path: impl AsRef<Path>,
        checkpoint: CheckpointId,
    ) -> Result<StoredCheckpoint, SessionStoreError> {
        let connection =
            Self::open_inspection(path.as_ref())?.ok_or(SessionStoreError::NotFound)?;
        let snapshot = connection.unchecked_transaction().map_err(map_sqlite)?;
        admit_inspection_snapshot(&snapshot)?;
        load_checkpoint(&snapshot, checkpoint)
    }

    /// Reads one exact branch without opening a writable runtime or changing its
    /// boot domain, lease, schema, or durable rows. `SQLite` may still update
    /// transient shared-memory reader coordination for a live WAL.
    ///
    /// # Errors
    ///
    /// Returns a typed shape, lookup, integrity, or storage failure.
    pub fn inspect_branch(
        path: impl AsRef<Path>,
        session: SessionRef,
    ) -> Result<LoadedBranch, SessionStoreError> {
        let connection =
            Self::open_inspection(path.as_ref())?.ok_or(SessionStoreError::NotFound)?;
        // One snapshot binds the schema check, branch head and all referenced
        // objects. A concurrent writer cannot make these reads span commits.
        let snapshot = connection.unchecked_transaction().map_err(map_sqlite)?;
        admit_inspection_snapshot(&snapshot)?;
        load_branch(&snapshot, session)
    }

    /// Inspects the head row and its addressed diagnostic metadata without history materialization.
    /// This validates only the metadata and obligation paths, not the whole checkpoint graph.
    ///
    /// # Errors
    /// Returns typed lookup, shape, integrity or storage failures.
    pub fn inspect_branch_metadata(
        path: impl AsRef<Path>,
        selector: SessionRef,
    ) -> Result<(BranchHead, RuntimeCheckpointInspection), SessionStoreError> {
        let connection =
            Self::open_inspection(path.as_ref())?.ok_or(SessionStoreError::NotFound)?;
        let snapshot = connection.unchecked_transaction().map_err(map_sqlite)?;
        admit_inspection_snapshot(&snapshot)?;
        branch_metadata(&snapshot, selector)
    }

    /// Read current branch revisions through one read-only snapshot.
    /// Missing or unreadable individual branches have no revision.
    ///
    /// # Errors
    /// The database or snapshot could not be admitted for inspection.
    pub fn inspect_branch_revisions(
        path: impl AsRef<Path>,
        selectors: &[SessionRef],
    ) -> Result<Vec<Option<u64>>, SessionStoreError> {
        let connection =
            Self::open_inspection(path.as_ref())?.ok_or(SessionStoreError::NotFound)?;
        let snapshot = connection.unchecked_transaction().map_err(map_sqlite)?;
        admit_inspection_snapshot(&snapshot)?;
        Ok(selectors
            .iter()
            .map(|selector| {
                branch_row(&snapshot, *selector)
                    .ok()
                    .flatten()
                    .map(|branch| branch.head.revision().get())
            })
            .collect())
    }
}

pub(super) fn branch_metadata(
    snapshot: &Connection,
    selector: SessionRef,
) -> Result<(BranchHead, RuntimeCheckpointInspection), SessionStoreError> {
    let session = live_session(snapshot, selector.session_id())?;
    let (branch, row) =
        branch_row_with_checkpoint(snapshot, selector)?.ok_or(SessionStoreError::NotFound)?;
    let reference = row.reference;
    let inspection = plakous_agent::internal::RuntimeCheckpointCodec::inspect_metadata(
        reference.checkpoint_id(),
        reference.integrity(),
        reference.root(),
        |address| load_object(snapshot, address)?.ok_or_else(SessionStoreError::corrupt),
        bundle_error,
    )?;
    let topology = inspection.topology;
    if row.revision != branch.head.revision()
        || row.selector != selector
        || topology.session_id != selector.session_id()
        || topology.branch_id != selector.branch_id()
        || topology.checkpoint_id != reference.checkpoint_id()
        || topology.branch_revision != branch.head.revision().get()
        || topology.parent_checkpoint_id != row.parent
        || topology.fork != row.lineage
        || topology.lease_fence != row.fence
        || topology.commit_identity != row.commit
        || inspection.durability != row.durability
        || inspection.durability != session.durability()
        || matches!(
            inspection.lifecycle,
            plakous_agent::internal::RuntimeLifecycle::Working { .. }
        ) != (row.kind == RuntimeCheckpointKind::Working)
    {
        return Err(SessionStoreError::corrupt());
    }
    Ok((branch.head, inspection))
}
