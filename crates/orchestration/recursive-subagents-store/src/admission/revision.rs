use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use rusqlite::{Connection, Transaction, TransactionBehavior};

use super::{AdmissionError, Revision, Schema, Status};

impl Schema {
    pub(super) fn steps(&self) -> impl Iterator<Item = (i64, &'static Revision)> + '_ {
        (self.generation..).zip(self.revisions)
    }

    /// Copy a live source using a read-only SQLite connection, then exercise
    /// upgrades on the scratch file only. Absent stores stay absent.
    pub fn rehearse(&self, source: &Path, scratch: &Path) -> Result<(), AdmissionError> {
        if let Some(connection) = self.open_reader(source)? {
            self.copy(&connection, scratch)?;
            if let Some(upgrade) = self.lock_upgrade(scratch)? {
                upgrade.commit_with(|_, _| {})?;
            }
        }
        Ok(())
    }

    /// Inspect and, if needed, lock a stopped store's pending revisions.
    /// The caller must already hold exclusive product-owner admission.
    pub fn lock_upgrade<'a>(
        &'a self,
        path: &'a Path,
    ) -> Result<Option<LockedUpgrade<'a>>, AdmissionError> {
        if !matches!(self.inspect(path)?, Status::Pending(_)) {
            return Ok(None);
        }
        let mut connection = self.writer(path, false)?;
        connection
            .busy_timeout(std::time::Duration::ZERO)
            .map_err(|_| self.unavailable("upgrade timeout could not be configured"))?;
        self.configure_durability(&connection)?;
        let mode: String = connection
            .query_row("PRAGMA locking_mode=EXCLUSIVE", [], |row| row.get(0))
            .map_err(|error| self.sql_error(&error, "could not set exclusive locking mode"))?;
        if mode != "exclusive" {
            return Err(self.unavailable("did not retain exclusive locking mode"));
        }
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Exclusive)
            .map_err(|error| self.sql_error(&error, "could not acquire exclusive upgrade lock"))?;
        let status = self.classify(&transaction)?;
        transaction
            .commit()
            .map_err(|error| self.sql_error(&error, "could not retain exclusive upgrade lock"))?;
        let Status::Pending(source) = status else {
            return Ok(None);
        };
        // EXCLUSIVE locking mode retains the lock between transactions and
        // VACUUM INTO cannot run inside one. No other writer can slip between.
        Ok(Some(LockedUpgrade {
            schema: self,
            connection,
            path,
            source,
        }))
    }

    fn backup(&self, conn: &Connection, path: &Path, source: i64) -> Result<(), AdmissionError> {
        let parent = path
            .parent()
            .ok_or_else(|| self.unavailable("backup directory is missing"))?;
        let backup = parent.join(format!("{}.pre-revision-{source}.sqlite3", self.stem));
        let pending = parent.join(format!(".{}-backup-{}", self.stem, uuid::Uuid::new_v4()));
        self.copy(conn, &pending)?;
        if fs::rename(&pending, &backup).is_err() {
            let _ = fs::remove_file(&pending);
            return Err(self.unavailable("pre-upgrade copy could not be published"));
        }
        File::open(parent)
            .and_then(|file| file.sync_all())
            .map_err(|_| self.unavailable("pre-upgrade copy directory could not be synced"))?;
        Ok(())
    }

    fn copy(&self, conn: &Connection, path: &Path) -> Result<(), AdmissionError> {
        let name = path
            .to_str()
            .ok_or_else(|| self.unavailable("copy path is not UTF-8"))?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|_| self.unavailable("copy file could not be created"))?;
        let result = conn
            .execute("VACUUM INTO ?1", [name])
            .map_err(|error| self.sql_error(&error, "consistent copy could not be made"))
            .and_then(|_| {
                file.sync_all()
                    .map_err(|_| self.unavailable("copy file could not be synced"))
            });
        if result.is_err() {
            let _ = fs::remove_file(path);
        }
        result
    }

    pub(super) fn apply_steps(
        &self,
        transaction: &Transaction<'_>,
        from: i64,
    ) -> Result<(), AdmissionError> {
        self.validate_revision(transaction, from)
            .map_err(AdmissionError::Unavailable)?;
        for (source, step) in self.steps().skip_while(|(source, _)| *source < from) {
            transaction
                .execute_batch(step.sql)
                .map_err(|error| self.sql_error(&error, "could not apply revision SQL"))?;
            let changed = transaction
                .execute(
                    &format!(
                        "UPDATE {}_schema SET fingerprint=?1 WHERE singleton=1",
                        self.stem
                    ),
                    [step.fingerprint],
                )
                .map_err(|error| self.sql_error(&error, "could not record revision fingerprint"))?;
            if changed != 1 {
                return Err(self.unavailable("did not update exactly one identity row"));
            }
            self.validate_revision(transaction, source + 1)
                .map_err(AdmissionError::Unavailable)?;
        }
        transaction
            .pragma_update(None, "user_version", self.version())
            .map_err(|error| self.sql_error(&error, "could not record schema revision"))
    }
}

/// A stopped-store revision path with its exclusive SQLite lock held.
pub struct LockedUpgrade<'a> {
    schema: &'a Schema,
    connection: Connection,
    path: &'a Path,
    source: i64,
}

impl<'a> LockedUpgrade<'a> {
    /// Publish and sync a consistent copy, preserving the lock until commit.
    pub fn backup(self) -> Result<BackedUpUpgrade<'a>, AdmissionError> {
        self.schema
            .backup(&self.connection, self.path, self.source)?;
        Ok(BackedUpUpgrade(self))
    }

    fn commit_with(mut self, cut: impl Fn(&str, &str)) -> Result<(), AdmissionError> {
        let schema = self.schema;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Exclusive)
            .map_err(|error| schema.sql_error(&error, "could not start revision transaction"))?;
        schema.apply_steps(&transaction, self.source)?;
        cut(schema.stem, "before-commit");
        transaction
            .commit()
            .map_err(|error| schema.sql_error(&error, "could not commit revisions"))?;
        cut(schema.stem, "after-commit");
        Ok(())
    }
}

/// An exclusive upgrade whose pre-step copy is durable.
pub struct BackedUpUpgrade<'a>(LockedUpgrade<'a>);

impl BackedUpUpgrade<'_> {
    /// Atomically apply the store's steps and release the lock.
    pub fn commit(self, at_commit: impl Fn(&str, &str)) -> Result<(), AdmissionError> {
        self.0.commit_with(at_commit)
    }
}
