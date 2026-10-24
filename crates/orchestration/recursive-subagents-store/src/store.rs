//! [`SqliteTreeStore`]: the `TreeStore` port over `subagents.sqlite3`.
//!
//! One connection, held for the life of the runtime that owns it, rather than
//! reopened per save. Every publish is one `Immediate` transaction and a
//! compare-and-swap on the tree revision. A save writes only the rows that
//! differ from the tree this connection last committed; with no such image it
//! replaces the tree's rows wholesale. Both leave the same rows.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use plakous_recursive_subagents::tree::{
    PersistedTree, ProductAgentTree, Revision, TreeStore, TreeStoreError, fence_revision,
};
use rusqlite::{ErrorCode, OptionalExtension};

use crate::database::{OpenError, SubagentsDatabase, TreeInspection};
use crate::from_i64;
use crate::image::TreeImage;
use crate::rows::{
    delete_agent_tree_rows, insert_agent_tree_rows, load_agent_tree, swap_agent_tree_header,
};

/// The tree's database as a [`TreeStore`].
pub struct SqliteTreeStore {
    writer: Mutex<Writer>,
}

/// The connection and, per tree, the image of its last acknowledged save.
struct Writer {
    database: SubagentsDatabase,
    images: BTreeMap<String, TreeImage>,
}

impl SqliteTreeStore {
    /// Reads one existing tree's exact persisted rows without a writer or
    /// in-memory reopen transition.
    pub fn inspect(database: &Path, tree_id: &str) -> Result<TreeInspection, OpenError> {
        SubagentsDatabase::inspect_tree(database, tree_id)
    }

    /// Opens `database`, creating and versioning it when it is new.
    pub fn open(database: &Path) -> Result<Self, OpenError> {
        Ok(Self {
            writer: Mutex::new(Writer {
                database: SubagentsDatabase::open(database)?,
                images: BTreeMap::new(),
            }),
        })
    }

    /// Deletes every row of one tree, compare-and-swapped on the revision the
    /// caller inspected. A tree already absent is not an error, so an
    /// interrupted deletion can be run again.
    ///
    /// # Errors
    ///
    /// Refuses a tree whose revision moved since it was inspected, and an
    /// unreachable store.
    pub fn delete(&self, tree_id: &str, expected_revision: u64) -> Result<(), TreeStoreError> {
        self.with_writer(|writer| {
            writer.images.remove(tree_id);
            let tx = writer
                .database
                .connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|error| sqlite_unavailable("could not begin agent tree delete", &error))?;
            match stored_revision(&tx, tree_id)? {
                None => return Ok(()),
                Some(stored) if stored != expected_revision => {
                    return Err(TreeStoreError::stale(stored, expected_revision));
                }
                Some(_) => {}
            }
            delete_agent_tree_rows(&tx, tree_id).map_err(TreeStoreError::unavailable)?;
            tx.commit()
                .map_err(|error| sqlite_unavailable("could not durably delete agent tree", &error))
        })
    }

    fn with_database<T>(
        &self,
        work: impl FnOnce(&SubagentsDatabase) -> Result<T, TreeStoreError>,
    ) -> Result<T, TreeStoreError> {
        self.with_writer(|writer| work(&writer.database))
    }

    #[cfg(all(test, feature = "commit-trace"))]
    fn with_database_mut<T>(
        &self,
        work: impl FnOnce(&mut SubagentsDatabase) -> Result<T, TreeStoreError>,
    ) -> Result<T, TreeStoreError> {
        self.with_writer(|writer| work(&mut writer.database))
    }

    fn with_writer<T>(
        &self,
        work: impl FnOnce(&mut Writer) -> Result<T, TreeStoreError>,
    ) -> Result<T, TreeStoreError> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| TreeStoreError::unavailable("tree store lock is poisoned"))?;
        #[cfg(feature = "commit-trace")]
        let _settle = plakous_store::commit_trace::SettleGuard;
        work(&mut writer)
    }
}

impl Writer {
    /// One publish. The image is taken out for the whole save and put back
    /// only by an acknowledged commit, so after any refused, failed or
    /// ambiguous save the next one replaces the tree wholesale.
    fn save(&mut self, tree: &ProductAgentTree) -> Result<(), TreeStoreError> {
        let tree_id = tree.tree_id.as_str();
        let image = self.images.remove(tree_id);
        let tx = self
            .database
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| sqlite_unavailable("could not begin agent tree save", &error))?;
        // The `Immediate` transaction holds the write lock from here through
        // the commit, so the version read, the compare and the swap are one
        // step.
        let data_version = tx
            .query_row("PRAGMA data_version", [], |row| row.get::<_, i64>(0))
            .map_err(|error| {
                sqlite_unavailable("could not read agent tree data version", &error)
            })?;
        let image = match image {
            Some(mut image) if image.is_current(data_version) => {
                let swapped =
                    swap_agent_tree_header(&tx, tree).map_err(TreeStoreError::unavailable)?;
                if swapped != 1 {
                    let stored = stored_revision(&tx, tree_id)?;
                    fence_revision(stored, tree.stored_revision, tree.revision)?;
                    return Err(TreeStoreError::stale(stored.unwrap_or(0), tree.revision));
                }
                let delta = image
                    .write_changes(&tx, tree)
                    .map_err(TreeStoreError::unavailable)?;
                commit(tx)?;
                delta.fold_into(&mut image);
                image
            }
            _ => {
                fence_revision(
                    stored_revision(&tx, tree_id)?,
                    tree.stored_revision,
                    tree.revision,
                )?;
                delete_agent_tree_rows(&tx, tree_id).map_err(TreeStoreError::unavailable)?;
                insert_agent_tree_rows(&tx, tree).map_err(TreeStoreError::unavailable)?;
                commit(tx)?;
                TreeImage::of(tree, data_version)
            }
        };
        self.images.insert(tree.tree_id.clone(), image);
        Ok(())
    }
}

fn stored_revision(
    tx: &rusqlite::Transaction<'_>,
    tree_id: &str,
) -> Result<Option<u64>, TreeStoreError> {
    tx.query_row(
        "SELECT revision FROM agent_trees WHERE tree_id=?1",
        [tree_id],
        |row| from_i64(row.get(0)?),
    )
    .optional()
    .map_err(|_| TreeStoreError::corrupt("agent tree revision is invalid"))
}

fn commit(tx: rusqlite::Transaction<'_>) -> Result<(), TreeStoreError> {
    tx.commit()
        .map_err(|error| sqlite_unavailable("could not durably commit agent tree", &error))
}

impl TreeStore for SqliteTreeStore {
    fn exists(&self, tree_id: &str) -> Result<bool, TreeStoreError> {
        self.with_database(|database| {
            let found = database
                .connection
                .query_row(
                    "SELECT 1 FROM agent_trees WHERE tree_id=?1",
                    [tree_id],
                    |_| Ok(()),
                )
                .optional()
                .map_err(|_| {
                    TreeStoreError::unavailable("could not inspect agent tree presence")
                })?;
            Ok(found.is_some())
        })
    }

    fn ids(&self) -> Result<Vec<String>, TreeStoreError> {
        self.with_database(|database| {
            let mut statement = database
                .connection
                .prepare("SELECT tree_id FROM agent_trees ORDER BY tree_id ASC")
                .map_err(|_| TreeStoreError::unavailable("could not query agent trees"))?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|_| TreeStoreError::unavailable("could not query agent trees"))?;
            let mut ids = Vec::new();
            for row in rows {
                ids.push(row.map_err(|_| TreeStoreError::corrupt("agent tree row is invalid"))?);
            }
            Ok(ids)
        })
    }

    fn save(&self, tree: &ProductAgentTree) -> Result<Revision, TreeStoreError> {
        self.with_writer(|writer| writer.save(tree))?;
        Ok(Revision::published(tree.revision))
    }

    fn load(&self, tree_id: &str) -> Result<Option<PersistedTree>, TreeStoreError> {
        self.with_database(|database| {
            let tx = database
                .connection
                .unchecked_transaction()
                .map_err(|_| TreeStoreError::unavailable("could not start agent tree read"))?;
            let tree = load_agent_tree(&tx, tree_id).map_err(TreeStoreError::corrupt)?;
            tx.commit()
                .map_err(|_| TreeStoreError::unavailable("could not finish agent tree read"))?;
            Ok(tree)
        })
    }
}

fn sqlite_unavailable(phase: &'static str, error: &rusqlite::Error) -> TreeStoreError {
    let category = match error.sqlite_error_code() {
        Some(ErrorCode::DiskFull) => "disk_full",
        Some(ErrorCode::DatabaseBusy) => "database_busy",
        Some(ErrorCode::DatabaseLocked) => "database_locked",
        Some(ErrorCode::SystemIoFailure) => "io_failure",
        Some(ErrorCode::CannotOpen) => "cannot_open",
        Some(ErrorCode::ReadOnly) => "read_only",
        Some(_) => "other_sqlite_failure",
        None => "sqlite_api_failure",
    };
    TreeStoreError::unavailable(format!("{phase}: {category}"))
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "commit-trace")]
    #[test]
    fn commit_trace_frameless_tree_helpers_release_for_another_thread()
    -> Result<(), Box<dyn std::error::Error>> {
        use plakous_store::commit_trace::{KILL_ENV, TRACE_ENV};
        let root = tempfile::tempdir()?;
        let trace = root.path().join("trace.jsonl");
        let output = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "store::tests::commit_trace_frameless_tree_child",
                "--nocapture",
            ])
            .env("PLAK_SW1_TREE_FRAMELESS_ROOT", root.path())
            .env(TRACE_ENV, &trace)
            .env_remove(KILL_ENV)
            .output()?;
        assert!(output.status.success(), "{output:?}");
        let lines: Vec<serde_json::Value> = std::fs::read_to_string(trace)?
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["ordinal"], 1);
        assert_eq!(lines[1]["ordinal"], 2);
        assert!(lines.iter().all(|line| line["store"] == "tree"));
        Ok(())
    }

    #[cfg(feature = "commit-trace")]
    #[test]
    fn commit_trace_frameless_tree_child() -> Result<(), Box<dyn std::error::Error>> {
        use super::{SqliteTreeStore, TreeStoreError};
        let Some(root) = std::env::var_os("PLAK_SW1_TREE_FRAMELESS_ROOT") else {
            return Ok(());
        };
        let path = std::path::PathBuf::from(root).join("tree.sqlite3");
        let store = SqliteTreeStore::open(&path)?;
        let no_op = |database: &SubagentsDatabase| {
            database
                .connection
                .execute_batch("BEGIN IMMEDIATE; SELECT singleton FROM subagents_schema; COMMIT;")
                .map_err(|error| TreeStoreError::unavailable(error.to_string()))
        };
        store
            .with_database_mut(|database| no_op(database))
            .map_err(|error| std::io::Error::other(error.detail))?;
        store
            .with_database(no_op)
            .map_err(|error| std::io::Error::other(error.detail))?;
        std::thread::spawn(move || -> Result<(), OpenError> {
            let peer = SqliteTreeStore::open(&path)?;
            peer.with_database_mut(|database| {
                database
                    .connection
                    .execute("UPDATE subagents_schema SET instance_id=instance_id", [])
                    .map(|_| ())
                    .map_err(|error| TreeStoreError::unavailable(error.to_string()))
            })
            .map_err(|error| OpenError::unavailable_text(error.detail))
        })
        .join()
        .map_err(|_| std::io::Error::other("peer tree panicked"))??;
        Ok(())
    }
    use super::*;

    #[test]
    #[allow(clippy::unwrap_used, reason = "isolated SQLite inspection fixture")]
    fn inspection_distinguishes_missing_store_and_tree_without_creating_state() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("subagents.sqlite3");
        assert!(matches!(
            SqliteTreeStore::inspect(&database, "root").unwrap(),
            TreeInspection::StoreAbsent
        ));
        assert!(!database.exists());
        let absent_parent = root.path().join("missing").join("subagents.sqlite3");
        assert!(matches!(
            SqliteTreeStore::inspect(&absent_parent, "root").unwrap(),
            TreeInspection::StoreAbsent
        ));
        assert!(!absent_parent.parent().unwrap().exists());

        let store = SqliteTreeStore::open(&database).unwrap();
        let before: (i64, String) = store
            .with_database(|database| {
                database
                    .connection
                    .query_row(
                        "SELECT (SELECT count(*) FROM agent_trees), fingerprint FROM subagents_schema WHERE singleton=1",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .map_err(|_| TreeStoreError::unavailable("could not inspect test snapshot"))
            })
            .unwrap();
        assert!(matches!(
            SqliteTreeStore::inspect(&database, "root").unwrap(),
            TreeInspection::TreeAbsent
        ));
        let after: (i64, String) = store
            .with_database(|database| {
                database
                    .connection
                    .query_row(
                        "SELECT (SELECT count(*) FROM agent_trees), fingerprint FROM subagents_schema WHERE singleton=1",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .map_err(|_| TreeStoreError::unavailable("could not inspect test snapshot"))
            })
            .unwrap();
        assert_eq!(after, before);
    }

    #[test]
    fn save_failure_identifies_sqlite_cause_without_backend_text() {
        for (code, category) in [
            (rusqlite::ffi::SQLITE_FULL, "disk_full"),
            (rusqlite::ffi::SQLITE_BUSY, "database_busy"),
        ] {
            let error = rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                Some("private database path and SQL".to_owned()),
            );
            let mapped = sqlite_unavailable("could not durably commit agent tree", &error);
            assert_eq!(
                mapped.detail,
                format!("could not durably commit agent tree: {category}")
            );
        }
    }
}
