//! Read-only native checkpoint inspection; never owns a runtime or lease.
use super::{
    BranchHead, CheckpointId, Connection, LoadedBranch, RuntimeCheckpointInspection,
    RuntimeCheckpointKind, SessionHead, SessionRef, SessionStoreError, SqliteSessionStore,
    StoredCheckpoint, admit_inspection_snapshot, branch_row, branch_row_with_checkpoint,
    bundle_error, live_session, load_branch, load_checkpoint, load_object, map_sqlite,
};
use std::path::Path;

/// Host upgrade's read-only view of a single runtime transaction.
#[doc(hidden)]
pub struct PreservationReader<'a>(&'a Connection);

impl PreservationReader<'_> {
    /// Run a preservation read over one admitted snapshot.
    ///
    /// # Errors
    /// Returns the usual read-only runtime admission failures.
    pub fn inspect<T>(
        path: &Path,
        read: impl FnOnce(&PreservationReader<'_>) -> T,
    ) -> Result<T, SessionStoreError> {
        let connection =
            SqliteSessionStore::open_inspection(path)?.ok_or(SessionStoreError::NotFound)?;
        let snapshot = connection.unchecked_transaction().map_err(map_sqlite)?;
        admit_inspection_snapshot(&snapshot)?;
        Ok(read(&PreservationReader(&snapshot)))
    }

    /// Borrows the admitted reader for preservation row hashing.
    #[must_use]
    pub const fn connection(&self) -> &Connection {
        self.0
    }

    /// Verify the native seal of a horizon retained in this snapshot.
    ///
    /// # Errors
    /// Refuses a missing, malformed or unsealed horizon.
    pub fn validate_horizon(
        &self,
        session: super::SessionId,
        branch: plakous_agent::internal::BranchId,
    ) -> Result<(), SessionStoreError> {
        super::rows::load_horizon(self.0, SessionRef::new(session, branch))?
            .ok_or_else(SessionStoreError::corrupt)?;
        Ok(())
    }

    /// Visit sealed Advanced records on the exact branch beyond the anchor.
    /// Retention never substitutes for evidence of acceptance.
    ///
    /// # Errors
    /// Refuses a malformed row, payload or seal.
    pub fn visit_advances(
        &self,
        session: super::SessionId,
        branch: plakous_agent::internal::BranchId,
        after: u64,
        visit: impl FnMut(plakous_agent::internal::FeedRecord) -> bool,
    ) -> Result<(), SessionStoreError> {
        super::feed::visit_advances(self.0, SessionRef::new(session, branch), after, visit)
    }
}

impl SqliteSessionStore {
    /// Checks the irreversible facts for a child-retirement abort.
    /// The caller must exclude every writer until the tree intent is removed.
    ///
    /// # Errors
    /// Returns a typed lookup, shape, or storage refusal.
    pub fn inspect_child_retirement_abort(
        path: impl AsRef<Path>,
        session: super::SessionId,
    ) -> Result<bool, SessionStoreError> {
        let connection =
            Self::open_inspection(path.as_ref())?.ok_or(SessionStoreError::NotFound)?;
        let snapshot = connection.unchecked_transaction().map_err(map_sqlite)?;
        admit_inspection_snapshot(&snapshot)?;
        let Some(head) = super::session_head(&snapshot, session)? else {
            return Ok(false);
        };
        let irreversible: bool = snapshot
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM archive_receipts WHERE session_id=?1)
             OR EXISTS(SELECT 1 FROM delete_tombstones WHERE session_id=?1)",
                [super::id(session)],
                |row| row.get(0),
            )
            .map_err(map_sqlite)?;
        Ok(!head.is_archived() && !irreversible)
    }

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
        Self::inspect_live_head_metadata(path, selector)
            .map(|(_, head, inspection)| (head, inspection))
    }

    /// Includes the owning live session revision in the same read snapshot.
    ///
    /// # Errors
    /// Returns the same failures as [`Self::inspect_branch_metadata`].
    pub fn inspect_live_head_metadata(
        path: impl AsRef<Path>,
        selector: SessionRef,
    ) -> Result<(SessionHead, BranchHead, RuntimeCheckpointInspection), SessionStoreError> {
        let connection =
            Self::open_inspection(path.as_ref())?.ok_or(SessionStoreError::NotFound)?;
        let snapshot = connection.unchecked_transaction().map_err(map_sqlite)?;
        admit_inspection_snapshot(&snapshot)?;
        let session = live_session(&snapshot, selector.session_id())?;
        let (head, inspection) = branch_metadata_for_session(&snapshot, selector, session)?;
        Ok((session, head, inspection))
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
    branch_metadata_for_session(snapshot, selector, session)
}

pub(super) fn branch_metadata_for_session(
    snapshot: &Connection,
    selector: SessionRef,
    session: SessionHead,
) -> Result<(BranchHead, RuntimeCheckpointInspection), SessionStoreError> {
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
