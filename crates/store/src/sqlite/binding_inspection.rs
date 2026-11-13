//! Read-only binding liveness metadata without runtime ownership.
use super::{
    BindingCheckpointInspection, SessionRef, SessionStoreError, SqliteSessionStore,
    admit_inspection_snapshot, load_checkpoint, map_sqlite,
};
use std::path::Path;

impl SqliteSessionStore {
    /// Loads retained branch checkpoints and liveness through one read-only snapshot.
    /// No boot, lease, runtime actor, recovery or write is opened.
    ///
    /// # Errors
    /// Returns schema, shape or read failures. An absent store is empty.
    pub fn inspect_binding_checkpoints(
        path: impl AsRef<Path>,
    ) -> Result<Vec<BindingCheckpointInspection>, SessionStoreError> {
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
                let stored = Some(load_checkpoint(&snapshot, id));
                Ok(BindingCheckpointInspection {
                    session,
                    checkpoint: id,
                    archived,
                    line_head: head,
                    stored,
                })
            })
            .collect()
    }
}
