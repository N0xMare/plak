//! [`SqliteTreeStore`]: the `TreeStore` port over `subagents.sqlite3`.
//!
//! One connection, held for the life of the runtime that owns it, rather than
//! reopened per save. Every publish is one `Immediate` transaction and a
//! compare-and-swap on the tree revision. A save writes only the rows that
//! differ from the current persisted tree. A stale image is rebuilt under
//! the same transaction before writing any delta.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex, Weak};

use plakous_recursive_subagents::tree::{
    CustodyClaim, PersistedTree, ProductAgentTree, Revision, TreeCustody, TreeStore,
    TreeStoreError, fence_revision,
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
    writer: Arc<Mutex<Writer>>,
    storage_waiting: std::sync::atomic::AtomicBool,
    /// The database file, canonical when it resolves: the custody scope.
    path: std::path::PathBuf,
}

/// The connection and, per tree, the image of its last acknowledged save.
struct Writer {
    database: SubagentsDatabase,
    images: BTreeMap<String, TreeImage>,
}

// One writing connection per canonical file in this process: two handles
// sharing a custody grant must also share its acknowledged image.
type Writers = BTreeMap<(u64, u64), Weak<Mutex<Writer>>>;

static WRITERS: LazyLock<Mutex<Writers>> = LazyLock::new(|| Mutex::new(BTreeMap::new()));

impl SqliteTreeStore {
    /// Reads one existing tree's exact persisted rows without a writer or
    /// in-memory reopen transition.
    pub fn inspect(database: &Path, tree_id: &str) -> Result<TreeInspection, OpenError> {
        SubagentsDatabase::inspect_tree(database, tree_id)
    }

    /// Opens `database`, creating and versioning it when it is new.
    pub fn open(database: &Path) -> Result<Self, OpenError> {
        let mut writers = WRITERS
            .lock()
            .map_err(|_| OpenError::unavailable_text("tree store writer registry is poisoned"))?;
        writers.retain(|_, writer| writer.strong_count() > 0);
        let key = std::fs::symlink_metadata(database)
            .ok()
            .filter(std::fs::Metadata::is_file)
            .map(|metadata| (metadata.dev(), metadata.ino()));
        let existing = key.and_then(|key| writers.get(&key).and_then(Weak::upgrade));
        let writer = if let Some(writer) = existing {
            writer
                .lock()
                .map_err(|_| OpenError::unavailable_text("tree store lock is poisoned"))?
                .database
                .readmit()?;
            writer
        } else {
            let database_connection = SubagentsDatabase::open(database)?;
            let metadata = std::fs::metadata(database).map_err(|_| {
                OpenError::unavailable_text("tree store file identity is unavailable")
            })?;
            let key = (metadata.dev(), metadata.ino());
            let writer = Arc::new(Mutex::new(Writer {
                database: database_connection,
                images: BTreeMap::new(),
            }));
            writers.insert(key, Arc::downgrade(&writer));
            writer
        };
        let path = std::fs::canonicalize(database)
            .map_err(|_| OpenError::unavailable_text("tree store path is unavailable"))?;
        Ok(Self {
            path,
            storage_waiting: std::sync::atomic::AtomicBool::new(false),
            writer,
        })
    }

    /// Deletes every row of one tree, compare-and-swapped on the revision the
    /// caller inspected and fenced by `claim`, which must hold the tree's
    /// custody; the custody record goes with the rows, in the same
    /// transaction. A tree already absent is not an error, so an interrupted
    /// deletion can be run again.
    ///
    /// # Errors
    ///
    /// Refuses a claim that does not hold the tree, a tree whose revision
    /// moved since it was inspected, and an unreachable store.
    pub fn delete(
        &self,
        tree_id: &str,
        expected_revision: u64,
        claim: &CustodyClaim,
    ) -> Result<(), TreeStoreError> {
        self.with_writer(|writer| {
            writer.images.remove(tree_id);
            let tx = writer
                .database
                .connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|error| sqlite_unavailable("could not begin agent tree delete", &error))?;
            crate::custody::prove_in(&tx, tree_id, claim)?;
            match stored_revision(&tx, tree_id)? {
                None => {}
                Some(stored) if stored != expected_revision => {
                    return Err(TreeStoreError::stale(stored, expected_revision));
                }
                Some(_) => delete_agent_tree_rows(&tx, tree_id)?,
            }
            crate::custody::delete_record(&tx, tree_id)?;
            tx.commit()
                .map_err(|error| sqlite_unavailable("could not durably delete agent tree", &error))
        })
    }

    /// The writer connection, for a step the tree save does not own (the
    /// custody record), under the same lock.
    pub(crate) fn with_connection<T, E: From<TreeStoreError>>(
        &self,
        work: impl FnOnce(&mut rusqlite::Connection) -> Result<T, E>,
    ) -> Result<T, E> {
        self.with_writer(|writer| Ok(work(&mut writer.database.connection)))
            .unwrap_or_else(|error| Err(error.into()))
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
    /// ambiguous save the next one rebuilds its baseline from current rows.
    fn save(
        &mut self,
        tree: &ProductAgentTree,
        claim: &CustodyClaim,
    ) -> Result<(), TreeStoreError> {
        let tree_id = tree.tree_id.as_str();
        let image = self.images.remove(tree_id);
        let tx = self
            .database
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| sqlite_unavailable("could not begin agent tree save", &error))?;
        // The custody proof and the write are one transaction: a claim
        // another take superseded writes nothing.
        crate::custody::prove_in(&tx, tree_id, claim)?;
        // The `Immediate` transaction holds the write lock from here through
        // the commit, so the version read, the compare and the swap are one
        // step.
        let data_version = tx
            .query_row("PRAGMA data_version", [], |row| row.get::<_, i64>(0))
            .map_err(|error| {
                sqlite_unavailable("could not read agent tree data version", &error)
            })?;
        let stored = stored_revision(&tx, tree_id)?;
        fence_revision(stored, tree.stored_revision, tree.revision)?;
        let image = image
            .and_then(|mut image| image.validate(data_version, stored, claim).then_some(image));
        let image = if let Some(image) = image {
            Some(image)
        } else {
            load_agent_tree(&tx, tree_id)
                .map_err(TreeStoreError::corrupt)?
                .map(|baseline| TreeImage::of(baseline.rows(), data_version, claim))
        };
        let image = if let Some(mut image) = image {
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
            image.acknowledge(tree.revision, data_version);
            image
        } else {
            insert_agent_tree_rows(&tx, tree)?;
            commit(tx)?;
            TreeImage::of(tree, data_version, claim)
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
    fn delete(
        &self,
        tree_id: &str,
        expected_revision: u64,
        claim: &CustodyClaim,
    ) -> Result<(), TreeStoreError> {
        Self::delete(self, tree_id, expected_revision, claim)
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

    fn save(
        &self,
        tree: &ProductAgentTree,
        claim: &CustodyClaim,
    ) -> Result<Revision, TreeStoreError> {
        let result = self.with_writer(|writer| writer.save(tree, claim));
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
        self.with_writer(|writer| {
            writer.images.remove(tree_id);
            let tx = writer
                .database
                .connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|_| TreeStoreError::unavailable("could not start agent tree read"))?;
            let tree = load_agent_tree(&tx, tree_id).map_err(TreeStoreError::corrupt)?;
            let data_version = tx
                .query_row("PRAGMA data_version", [], |row| row.get::<_, i64>(0))
                .map_err(|_| {
                    TreeStoreError::unavailable("could not read agent tree data version")
                })?;
            let custody = crate::custody::read(&tx, tree_id)?;
            tx.commit()
                .map_err(|_| TreeStoreError::unavailable("could not finish agent tree read"))?;
            if let (Some(tree), Some(custody)) = (&tree, custody) {
                let grant = CustodyClaim {
                    tree_id: tree_id.to_owned(),
                    host: custody.holding.host,
                    fence: custody.fence,
                };
                writer.images.insert(
                    tree_id.to_owned(),
                    TreeImage::of(tree.rows(), data_version, &grant),
                );
            }
            Ok(tree)
        })
    }

    fn custody_port(&self) -> &dyn TreeCustody {
        self
    }
}

impl SqliteTreeStore {
    /// The canonical database path custody is scoped to.
    pub(crate) fn custody_path(&self) -> &Path {
        &self.path
    }

    /// A released tree must not stay resident in another handle's cache.
    pub(crate) fn forget_image(&self, tree_id: &str) {
        if let Ok(mut writer) = self.writer.lock() {
            writer.images.remove(tree_id);
        }
    }
}

pub(crate) fn sqlite_unavailable(phase: &'static str, error: &rusqlite::Error) -> TreeStoreError {
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
mod tests;

#[cfg(all(test, feature = "test-support"))]
mod cache_tests;
