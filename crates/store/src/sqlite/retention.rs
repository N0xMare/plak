//! Read-only per-session retention: the object bytes each session's retained
//! checkpoints reach, for an operator deciding what to delete.

use std::collections::HashMap;

use plakous_agent::internal::{SessionId, SessionStoreError};
use rusqlite::{Connection, params};

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
    /// Object and edge reads are keyset pages, and the checkpoint roots are
    /// one scan. Each page is its own read statement with no lookups per row,
    /// resolved and walked in memory: the cost grows with the retained objects
    /// and edges, and no read mark outlives one page. An object deleted
    /// between pages is skipped, so a store written meanwhile reports
    /// approximate figures. This is an operator inspection, not a runtime path.
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

/// Rows in one keyset page. Each page is its own statement, so the read mark
/// a concurrent checkpoint waits on lasts one page, not the table.
const RETENTION_PAGE: i64 = 10_000;

fn load_objects(
    c: &Connection,
    index: &mut HashMap<Address, usize>,
    sizes: &mut Vec<u64>,
) -> Result<(), SessionStoreError> {
    let mut after: Option<i64> = None;
    loop {
        let mut statement = match after {
            None => c.prepare(
                "SELECT id, kind, codec, digest, byte_len FROM runtime_objects \
                 ORDER BY id LIMIT ?1",
            ),
            Some(_) => c.prepare(
                "SELECT id, kind, codec, digest, byte_len FROM runtime_objects \
                 WHERE id > ?1 ORDER BY id LIMIT ?2",
            ),
        }
        .map_err(map_sqlite)?;
        let mut rows = match after {
            None => statement.query(params![RETENTION_PAGE]),
            Some(id) => statement.query(params![id, RETENTION_PAGE]),
        }
        .map_err(map_sqlite)?;
        let mut count = 0_i64;
        while let Some(row) = rows.next().map_err(map_sqlite)? {
            count += 1;
            let id: i64 = row.get(0).map_err(map_sqlite)?;
            after = Some(id);
            let address = (
                row.get(1).map_err(map_sqlite)?,
                row.get(2).map_err(map_sqlite)?,
                row.get(3).map_err(map_sqlite)?,
            );
            let bytes: i64 = row.get(4).map_err(map_sqlite)?;
            index.insert(address, sizes.len());
            sizes.push(u64::try_from(bytes).map_err(|_| SessionStoreError::corrupt())?);
        }
        if count < RETENTION_PAGE {
            break;
        }
    }
    Ok(())
}

fn load_edges(
    c: &Connection,
    index: &HashMap<Address, usize>,
    children: &mut [Vec<usize>],
) -> Result<(), SessionStoreError> {
    let mut cursor: Option<(i64, i64, Vec<u8>, i64)> = None;
    loop {
        let key = cursor.clone();
        let mut statement = if key.is_none() {
            c.prepare(
                "SELECT parent_kind, parent_codec, parent_digest, position, \
                        child_kind, child_codec, child_digest \
                 FROM runtime_object_edges \
                 ORDER BY parent_kind, parent_codec, parent_digest, position \
                 LIMIT ?1",
            )
        } else {
            c.prepare(
                "SELECT parent_kind, parent_codec, parent_digest, position, \
                        child_kind, child_codec, child_digest \
                 FROM runtime_object_edges \
                 WHERE (parent_kind, parent_codec, parent_digest, position) \
                       > (?1, ?2, ?3, ?4) \
                 ORDER BY parent_kind, parent_codec, parent_digest, position \
                 LIMIT ?5",
            )
        }
        .map_err(map_sqlite)?;
        let mut rows = match &key {
            None => statement.query(params![RETENTION_PAGE]),
            Some((kind, codec, digest, position)) => {
                statement.query(params![kind, codec, digest, position, RETENTION_PAGE])
            }
        }
        .map_err(map_sqlite)?;
        let mut count = 0_i64;
        while let Some(row) = rows.next().map_err(map_sqlite)? {
            count += 1;
            let parent_kind: i64 = row.get(0).map_err(map_sqlite)?;
            let parent_codec: i64 = row.get(1).map_err(map_sqlite)?;
            let parent_digest: Vec<u8> = row.get(2).map_err(map_sqlite)?;
            let position: i64 = row.get(3).map_err(map_sqlite)?;
            let parent = (parent_kind, parent_codec, parent_digest.clone());
            let child: Address = (
                row.get(4).map_err(map_sqlite)?,
                row.get(5).map_err(map_sqlite)?,
                row.get(6).map_err(map_sqlite)?,
            );
            cursor = Some((parent_kind, parent_codec, parent_digest, position));
            if let (Some(&parent), Some(&child)) = (index.get(&parent), index.get(&child)) {
                children[parent].push(child);
            }
        }
        if count < RETENTION_PAGE {
            break;
        }
    }
    Ok(())
}

fn session_retention(c: &Connection) -> Result<Vec<SqliteSessionRetention>, SessionStoreError> {
    let mut index: HashMap<Address, usize> = HashMap::new();
    let mut sizes = Vec::new();
    load_objects(c, &mut index, &mut sizes)?;
    let mut children = vec![Vec::new(); sizes.len()];
    load_edges(c, &index, &mut children)?;
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
