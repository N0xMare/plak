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
    storage_interrupt: Mutex<Option<std::sync::Arc<dyn Fn() -> bool + Send + Sync>>>,
    writer: Mutex<Writer>,
    storage_waiting: std::sync::atomic::AtomicBool,
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
            storage_interrupt: Mutex::new(None),
            storage_waiting: std::sync::atomic::AtomicBool::new(false),
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
            delete_agent_tree_rows(&tx, tree_id)?;
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
                let swapped = swap_agent_tree_header(&tx, tree)?;
                if swapped != 1 {
                    let stored = stored_revision(&tx, tree_id)?;
                    fence_revision(stored, tree.stored_revision, tree.revision)?;
                    // Unreachable while the image is current: an image means
                    // this connection committed the row and nothing removed it.
                    return Err(TreeStoreError::stale(stored.unwrap_or(0), tree.revision));
                }
                let delta = image.write_changes(&tx, tree)?;
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
                delete_agent_tree_rows(&tx, tree_id)?;
                insert_agent_tree_rows(&tx, tree)?;
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
    fn set_storage_interrupt(&self, probe: std::sync::Arc<dyn Fn() -> bool + Send + Sync>) {
        *self
            .storage_interrupt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(probe);
    }
    fn storage_wait_cancelled(&self) -> bool {
        self.storage_interrupt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|probe| probe())
    }
    fn recover_storage(&self) -> Result<(), TreeStoreError> {
        self.with_writer(|writer| {
            let tx = writer.database.connection.transaction()
                .map_err(|error| {
                    sqlite_unavailable("could not probe agent tree storage", &error)
                })?;
            tx.execute_batch("UPDATE subagents_schema SET fingerprint=fingerprint || 'x' WHERE singleton=1;
                UPDATE subagents_schema SET fingerprint=substr(fingerprint,1,length(fingerprint)-1) WHERE singleton=1;")
                .map_err(|error| sqlite_unavailable("could not probe agent tree storage", &error))?;
            tx.commit().map_err(|error| sqlite_unavailable("could not probe agent tree storage", &error))?;
            Ok(())
        })?;
        self.storage_waiting
            .store(false, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    fn storage_unavailable(&self) -> bool {
        self.storage_waiting
            .load(std::sync::atomic::Ordering::Acquire)
    }

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
        let result = self.with_writer(|writer| writer.save(tree));
        if result.is_ok() {
            self.storage_waiting
                .store(false, std::sync::atomic::Ordering::Release);
        } else if matches!(&result, Err(error) if error.kind == plakous_recursive_subagents::tree::TreeStoreErrorKind::StorageFull)
        {
            self.storage_waiting
                .store(true, std::sync::atomic::Ordering::Release);
        }
        result?;
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
    if error.sqlite_error_code() == Some(ErrorCode::DiskFull) {
        return TreeStoreError::storage_full();
    }
    let category = match error.sqlite_error_code() {
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

pub(crate) fn sqlite_write_error(error: rusqlite::Error, phase: String) -> TreeStoreError {
    let full = error.sqlite_error_code() == Some(ErrorCode::DiskFull);
    drop(error);
    if full {
        TreeStoreError::storage_full()
    } else {
        TreeStoreError::unavailable(phase)
    }
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
            .with_writer(|writer| no_op(&writer.database))
            .map_err(|error| std::io::Error::other(error.detail))?;
        store
            .with_database(no_op)
            .map_err(|error| std::io::Error::other(error.detail))?;
        std::thread::spawn(move || -> Result<(), OpenError> {
            let peer = SqliteTreeStore::open(&path)?;
            peer.with_writer(|writer| {
                writer
                    .database
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
        use plakous_recursive_subagents::tree::TreeStoreErrorKind;
        for (code, kind, detail) in [
            (
                rusqlite::ffi::SQLITE_FULL,
                TreeStoreErrorKind::StorageFull,
                "agent tree storage is full",
            ),
            (
                rusqlite::ffi::SQLITE_BUSY,
                TreeStoreErrorKind::Unavailable,
                "could not durably commit agent tree: database_busy",
            ),
        ] {
            let error = rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                Some("private database path and SQL".to_owned()),
            );
            let mapped = sqlite_unavailable("could not durably commit agent tree", &error);
            assert_eq!(mapped.kind, kind);
            assert_eq!(mapped.detail, detail);
            assert!(!mapped.detail.contains("private database path"));
        }
    }

    /// Only an acknowledged commit leaves an image behind: every refusal,
    /// failure and delete falls the next save back to the whole-tree path.
    #[cfg(feature = "test-support")]
    #[allow(clippy::unwrap_used, reason = "unit tests")]
    mod images {
        use std::sync::Arc;

        use plakous_recursive_subagents::protocol::{TopologyGrant, WriterMode};
        use plakous_recursive_subagents::tree::{
            FakeVcs, PersistedTree, ProductAgentTree, TreeStore, TreeStoreErrorKind,
        };

        use super::super::SqliteTreeStore;

        struct Fixture {
            _root: tempfile::TempDir,
            store: SqliteTreeStore,
            tree: ProductAgentTree,
        }

        /// A tree saved twice, so its image exists and the second save took
        /// the row-level path.
        fn fixture() -> Fixture {
            let root = tempfile::tempdir().unwrap();
            let store = SqliteTreeStore::open(&root.path().join("subagents.sqlite3")).unwrap();
            let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
                Arc::new(FakeVcs::new()),
                "tree",
                "root",
                "owner",
                root.path().to_owned(),
                "tuple",
                WriterMode::SingleLease,
            )
            .unwrap();
            for _ in 0..2 {
                tree.revision += 1;
                let revision = store.save(&tree).unwrap();
                tree.acknowledge_revision(revision);
            }
            assert_eq!(images(&store), 1);
            Fixture {
                _root: root,
                store,
                tree,
            }
        }

        fn images(store: &SqliteTreeStore) -> usize {
            store.writer.lock().unwrap().images.len()
        }

        #[test]
        fn a_stale_refusal_drops_the_image() {
            let Fixture { store, tree, .. } = fixture();
            let mut stale = tree.clone();
            stale.stored_revision += 1;
            stale.revision += 2;
            let error = store.save(&stale).unwrap_err();
            assert!(matches!(
                error.kind,
                TreeStoreErrorKind::StaleRevision { .. }
            ));
            assert_eq!(images(&store), 0);
        }

        #[test]
        fn a_foreign_key_refusal_drops_the_image() {
            let Fixture { store, tree, .. } = fixture();
            let mut grants = tree.grants().clone();
            grants.insert(
                "ghost".to_owned(),
                TopologyGrant {
                    max_nodes: 1,
                    max_children: 1,
                    ..TopologyGrant::default()
                },
            );
            let mut broken = PersistedTree::from_rows(
                tree.tree_id.clone(),
                tree.root_agent_id.clone(),
                tree.revision + 1,
                tree.host_generation,
                tree.state,
                tree.writer_mode,
                tree.selected_tuple.clone(),
                tree.launch_checkout.clone(),
                tree.snapshot.clone(),
                tree.nodes().clone(),
                grants,
                tree.attempts().clone(),
                tree.candidates().clone(),
                tree.workspaces().clone(),
                tree.messages().clone(),
                tree.operations().clone(),
            )
            .with_feed_rows(tree.commands().clone(), tree.feed_cursors().clone())
            .rows()
            .clone();
            broken.stored_revision = tree.revision;
            let error = store.save(&broken).unwrap_err();
            assert_eq!(error.kind, TreeStoreErrorKind::Unavailable, "{error}");
            assert_eq!(images(&store), 0);
        }

        #[test]
        fn a_failed_commit_drops_the_image() {
            let Fixture {
                store, mut tree, ..
            } = fixture();
            store
                .writer
                .lock()
                .unwrap()
                .database
                .connection
                .commit_hook(Some(|| true))
                .unwrap();
            tree.revision += 1;
            let error = store.save(&tree).unwrap_err();
            assert_eq!(
                error.detail,
                "could not durably commit agent tree: other_sqlite_failure"
            );
            assert_eq!(images(&store), 0);
            store
                .writer
                .lock()
                .unwrap()
                .database
                .connection
                .commit_hook(None::<fn() -> bool>)
                .unwrap();
            let revision = store.save(&tree).unwrap();
            tree.acknowledge_revision(revision);
            assert_eq!(images(&store), 1);
        }

        #[test]
        fn a_delete_drops_the_image() {
            let Fixture { store, tree, .. } = fixture();
            store.delete(&tree.tree_id, tree.revision).unwrap();
            assert_eq!(images(&store), 0);
            assert!(!store.exists(&tree.tree_id).unwrap());
        }
    }
}
