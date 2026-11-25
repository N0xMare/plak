//! Read-only binding liveness metadata without runtime ownership.
use super::{
    BindingCheckpointInspection, SessionRef, SessionStoreError, SqliteSessionStore,
    admit_inspection_snapshot, branch_row_with_checkpoint, bundle_error, id, load_checkpoint,
    load_object, map_sqlite,
};
use plakous_agent::internal::{
    CheckpointId, DurableSessionAddress, RuntimeCheckpointBinding, RuntimeCheckpointCodec,
};
use rusqlite::OptionalExtension as _;
use std::path::Path;

impl SqliteSessionStore {
    /// Visits the saved binding of each named live branch, using one read snapshot.
    /// An archived session is skipped; a missing branch is a per-head refusal.
    ///
    /// # Errors
    /// Returns database admission or storage failures.
    pub fn inspect_live_bindings(
        path: &Path,
        selectors: impl IntoIterator<Item = SessionRef>,
        mut visit: impl FnMut(
            Option<DurableSessionAddress>,
            Result<RuntimeCheckpointBinding, SessionStoreError>,
        ),
    ) -> Result<(), SessionStoreError> {
        let connection = Self::open_inspection(path)?.ok_or(SessionStoreError::NotFound)?;
        let snapshot = connection.unchecked_transaction().map_err(map_sqlite)?;
        admit_inspection_snapshot(&snapshot)?;
        let mut archived = snapshot
            .prepare("SELECT archived FROM sessions WHERE session_id=?1")
            .map_err(map_sqlite)?;
        for selector in selectors {
            if archived
                .query_row([id(selector.session_id())], |row| row.get::<_, bool>(0))
                .optional()
                .map_err(map_sqlite)?
                == Some(true)
            {
                continue;
            }
            let mut address = None;
            let binding = (|| {
                let (branch, row) = branch_row_with_checkpoint(&snapshot, selector)?
                    .ok_or(SessionStoreError::NotFound)?;
                let (line, generation) = super::lines::branch_line(&snapshot, selector)?
                    .ok_or_else(SessionStoreError::corrupt)?;
                super::lines::require_line_head(&snapshot, selector, line, generation)?;
                if row.selector != selector || row.revision != branch.head.revision() {
                    return Err(SessionStoreError::corrupt());
                }
                let reference = row.reference;
                address = Some(DurableSessionAddress::new(
                    selector.session_id(),
                    selector.branch_id(),
                    reference.checkpoint_id(),
                ));
                RuntimeCheckpointCodec::inspect_binding(
                    reference.checkpoint_id(),
                    reference.integrity(),
                    reference.root(),
                    |address| {
                        load_object(&snapshot, address)?.ok_or_else(SessionStoreError::corrupt)
                    },
                    bundle_error,
                )
            })();
            visit(address, binding);
        }
        Ok(())
    }

    /// Projects retained branch checkpoints and liveness through one read-only snapshot.
    /// No boot, lease, runtime actor, recovery or write is opened. Only heads
    /// that `decode` selects, from their liveness alone, load their checkpoint;
    /// the others are projected with no payload.
    ///
    /// # Errors
    /// Returns schema, shape or read failures. An absent store is empty.
    pub fn inspect_binding_checkpoints<T>(
        path: impl AsRef<Path>,
        mut decode: impl FnMut(SessionRef, CheckpointId, bool, bool) -> bool,
        mut project: impl FnMut(BindingCheckpointInspection) -> T,
    ) -> Result<Vec<T>, SessionStoreError> {
        let Some(connection) = Self::open_inspection(path.as_ref())? else {
            return Ok(Vec::new());
        };
        let snapshot = connection.unchecked_transaction().map_err(map_sqlite)?;
        admit_inspection_snapshot(&snapshot)?;
        let mut statement = snapshot
            .prepare("SELECT b.checkpoint_id,s.archived,EXISTS(SELECT 1 FROM lines l WHERE l.line_id=b.line_id AND l.head_session=b.session_id AND l.head_branch=b.branch_id),b.session_id,b.branch_id FROM branches b JOIN sessions s ON s.session_id=b.session_id ORDER BY b.checkpoint_id")
            .map_err(map_sqlite)?;
        let ids = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .map_err(map_sqlite)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite)?;
        ids.into_iter()
            .map(|(id, archived, head, session, branch)| {
                let id = id.parse().map_err(|_| SessionStoreError::corrupt())?;
                let session = SessionRef::new(
                    session.parse().map_err(|_| SessionStoreError::corrupt())?,
                    branch.parse().map_err(|_| SessionStoreError::corrupt())?,
                );
                let stored =
                    decode(session, id, archived, head).then(|| load_checkpoint(&snapshot, id));
                Ok(project(BindingCheckpointInspection {
                    session,
                    checkpoint: id,
                    archived,
                    line_head: head,
                    stored,
                }))
            })
            .collect()
    }
}
