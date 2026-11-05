//! Read-only per-session retention: the object bytes each session's retained
//! checkpoints reach, for an operator deciding what to delete.

use std::collections::HashMap;

use plakous_agent::internal::{SessionId, SessionStoreError};
use rusqlite::Connection;

use super::{SqlitePersistenceShape, SqliteSessionStore, classify_connection, map_sqlite, parse};

/// The retained checkpoints of one session and the object bytes they reach.
///
/// Objects are content-addressed and shared, so an object two sessions reach
/// counts toward both: the figures do not sum to the store's object bytes,
/// and deleting a session frees at most its figure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SqliteSessionRetention {
    /// The session.
    pub session_id: SessionId,
    /// Retained checkpoints of every branch of the session.
    pub checkpoints: u64,
    /// Accounted bytes of the objects those checkpoints reach.
    pub object_bytes: u64,
}

impl SqliteSessionStore {
    /// Reads every session's retention without opening a writable runtime,
    /// largest first. Returns `None` for a missing or fresh file.
    ///
    /// Three sequential table scans (objects, edges, checkpoint roots), each
    /// its own read statement with no lookups per row, resolved and walked in
    /// memory: the cost grows with the retained objects and edges, and no read
    /// snapshot outlives one scan. An object deleted between the scans is
    /// skipped, so a store written meanwhile reports approximate figures. This
    /// is an operator inspection, not a runtime path.
    ///
    /// # Errors
    ///
    /// Returns a typed shape, storage or integrity failure.
    pub fn inspect_session_retention(
        path: impl AsRef<std::path::Path>,
    ) -> Result<Option<Vec<SqliteSessionRetention>>, SessionStoreError> {
        let Some(connection) = Self::open_inspection(path.as_ref())? else {
            return Ok(None);
        };
        if classify_connection(&connection)? != SqlitePersistenceShape::Current {
            return Ok(None);
        }
        session_retention(&connection).map(Some)
    }
}

/// An object's content address: kind, codec and digest.
type Address = (i64, i64, Vec<u8>);

fn session_retention(c: &Connection) -> Result<Vec<SqliteSessionRetention>, SessionStoreError> {
    let mut index: HashMap<Address, usize> = HashMap::new();
    let mut sizes = Vec::new();
    let mut objects = c
        .prepare("SELECT kind, codec, digest, byte_len FROM runtime_objects")
        .map_err(map_sqlite)?;
    let mut rows = objects.query([]).map_err(map_sqlite)?;
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        let address = (
            row.get(0).map_err(map_sqlite)?,
            row.get(1).map_err(map_sqlite)?,
            row.get(2).map_err(map_sqlite)?,
        );
        let bytes: i64 = row.get(3).map_err(map_sqlite)?;
        index.insert(address, sizes.len());
        sizes.push(u64::try_from(bytes).map_err(|_| SessionStoreError::corrupt())?);
    }
    let mut children = vec![Vec::new(); sizes.len()];
    let mut edges = c
        .prepare(
            "SELECT parent_kind, parent_codec, parent_digest, child_kind, child_codec, child_digest
             FROM runtime_object_edges",
        )
        .map_err(map_sqlite)?;
    let mut rows = edges.query([]).map_err(map_sqlite)?;
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        let parent: Address = (
            row.get(0).map_err(map_sqlite)?,
            row.get(1).map_err(map_sqlite)?,
            row.get(2).map_err(map_sqlite)?,
        );
        let child: Address = (
            row.get(3).map_err(map_sqlite)?,
            row.get(4).map_err(map_sqlite)?,
            row.get(5).map_err(map_sqlite)?,
        );
        if let (Some(&parent), Some(&child)) = (index.get(&parent), index.get(&child)) {
            children[parent].push(child);
        }
    }
    let mut roots: Vec<(String, Option<usize>)> = Vec::new();
    let mut checkpoints = c
        .prepare(
            "SELECT session_id, root_kind, root_codec, root_digest FROM checkpoints
             ORDER BY session_id",
        )
        .map_err(map_sqlite)?;
    let mut rows = checkpoints.query([]).map_err(map_sqlite)?;
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        let root: Address = (
            row.get(1).map_err(map_sqlite)?,
            row.get(2).map_err(map_sqlite)?,
            row.get(3).map_err(map_sqlite)?,
        );
        roots.push((row.get(0).map_err(map_sqlite)?, index.get(&root).copied()));
    }
    // One visit mark per object, stamped with the session walking it, so an
    // object several checkpoints of one session reach counts once for it.
    let mut marks = vec![usize::MAX; sizes.len()];
    let mut stack = Vec::new();
    let mut retention = Vec::new();
    for (stamp, group) in roots.chunk_by(|a, b| a.0 == b.0).enumerate() {
        let mut object_bytes = 0_u64;
        for root in group.iter().filter_map(|(_, root)| *root) {
            if marks[root] == stamp {
                continue;
            }
            marks[root] = stamp;
            stack.push(root);
            while let Some(object) = stack.pop() {
                object_bytes = object_bytes.saturating_add(sizes[object]);
                for &child in &children[object] {
                    if marks[child] != stamp {
                        marks[child] = stamp;
                        stack.push(child);
                    }
                }
            }
        }
        retention.push(SqliteSessionRetention {
            session_id: parse(&group[0].0)?,
            checkpoints: u64::try_from(group.len()).unwrap_or(u64::MAX),
            object_bytes,
        });
    }
    retention.sort_by_key(|session| std::cmp::Reverse(session.object_bytes));
    Ok(retention)
}
