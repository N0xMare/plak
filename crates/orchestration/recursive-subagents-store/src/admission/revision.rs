use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;
use std::time::{Duration, Instant};

use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};

use super::{AdmissionError, Revision, Schema, Status};

impl Schema {
    pub(super) fn steps(&self) -> impl Iterator<Item = (i64, &'static Revision)> + '_ {
        (self.generation..).zip(self.revisions)
    }

    /// Copy a live source using a read-only SQLite connection, then exercise
    /// upgrades and prepare WAL on the scratch file only. Absent stores stay absent.
    pub fn rehearse(&self, source: &Path, scratch: &Path) -> Result<(), AdmissionError> {
        if let Some(connection) = self.open_reader(source)? {
            self.copy(&connection, scratch)?;
            if let Some(upgrade) = self.lock_upgrade(scratch)? {
                upgrade.commit_with(|_, _| {})?;
            }
            // VACUUM INTO leaves the copy in rollback-journal mode. Once
            // probes share it, even a read snapshot can block WAL promotion.
            self.configure_writer(&self.writer(scratch, false)?)?;
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
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match self.try_lock_upgrade(path) {
                // WAL lock promotions can return BUSY without calling SQLite's
                // busy handler. Retry on a new connection before any writes.
                Err(AdmissionError::Busy(_)) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                result => return result,
            }
        }
    }

    pub(super) fn try_lock_upgrade<'a>(
        &'a self,
        path: &'a Path,
    ) -> Result<Option<LockedUpgrade<'a>>, AdmissionError> {
        let mut connection = self.writer(path, false)?;
        connection
            .busy_timeout(Duration::ZERO)
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
        let staging = parent.join(format!(".{}-backup-{}", self.stem, uuid::Uuid::new_v4()));
        DirBuilder::new()
            .mode(0o700)
            .create(&staging)
            .map_err(|_| self.unavailable("pre-upgrade staging directory could not be created"))?;
        // SQLite creates the file itself; newer versions refuse even an
        // existing zero-byte file. Keep it private before publishing it.
        let pending = staging.join("copy.sqlite3");
        let result = self.copy(conn, &pending).and_then(|()| {
            fs::rename(&pending, &backup)
                .map_err(|_| self.unavailable("pre-upgrade copy could not be published"))
        });
        let _ = fs::remove_file(&pending);
        let _ = fs::remove_dir(&staging);
        result?;
        File::open(parent)
            .and_then(|file| file.sync_all())
            .map_err(|_| self.unavailable("pre-upgrade copy directory could not be synced"))?;
        Ok(())
    }

    fn copy(&self, conn: &Connection, path: &Path) -> Result<(), AdmissionError> {
        let path = self.resolved_path(path)?;
        let name = path
            .to_str()
            .ok_or_else(|| self.unavailable("copy path is not UTF-8"))?;
        if !matches!(path.symlink_metadata(), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        {
            return Err(self.unavailable("copy path is not available"));
        }
        let result = conn
            .execute("VACUUM INTO ?1", [name])
            .map_err(|error| self.sql_error(&error, "consistent copy could not be made"))
            .and_then(|_| {
                let file = File::open(&path)
                    .map_err(|_| self.unavailable("copy file could not be opened"))?;
                sync_copy(&file)
                    .map_err(|_| self.unavailable("copy file could not be protected and synced"))
            });
        if result.is_err() {
            let _ = fs::remove_file(&path);
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

fn sync_copy(file: &File) -> std::io::Result<()> {
    file.set_permissions(Permissions::from_mode(0o600))?;
    file.sync_all()
}

/// Copy a pinned read-only SQLite snapshot to a new private, synced file.
/// The caller stages the file in a private directory and owns cleanup on error.
///
/// # Errors
/// Refuses unsupported inputs, lock contention, budget or deadline exhaustion,
/// and I/O errors. An absent source uses no budget and creates no file.
pub fn copy_snapshot_bounded(
    source: &Path,
    destination: &Path,
    remaining: u64,
    deadline: Instant,
) -> Result<u64, AdmissionError> {
    let unavailable = |detail: &str| AdmissionError::Unavailable(detail.to_owned());
    match source.symlink_metadata() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Ok(meta) if meta.is_file() => {}
        _ => return Err(unavailable("backup source is not a regular file")),
    }
    let reader = Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|_| unavailable("could not open backup source"))?;
    let snapshot = reader
        .unchecked_transaction()
        .map_err(|_| unavailable("could not start backup snapshot"))?;
    let bytes: i64 = snapshot
        .query_row(
            "SELECT page_count * page_size FROM pragma_page_count, pragma_page_size",
            [],
            |row| row.get(0),
        )
        .map_err(|_| unavailable("could not read backup source size"))?;
    let bytes = u64::try_from(bytes).map_err(|_| unavailable("invalid backup source size"))?;
    if bytes == 0 {
        return Ok(0);
    }
    if bytes > remaining {
        return Err(unavailable("combined SQLite backup exceeds its byte bound"));
    }
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(destination)
        .map_err(|_| unavailable("could not create backup file"))?;
    let mut writer =
        Connection::open(destination).map_err(|_| unavailable("could not open backup file"))?;
    {
        let backup = Backup::new(&snapshot, &mut writer)
            .map_err(|_| unavailable("could not start SQLite backup"))?;
        loop {
            if Instant::now() >= deadline {
                return Err(unavailable("SQLite backup exceeded its time bound"));
            }
            match backup.step(256) {
                Ok(StepResult::Done) => break,
                Ok(StepResult::More) => {}
                _ => return Err(unavailable("could not copy SQLite backup snapshot")),
            }
        }
    }
    writer
        .close()
        .map_err(|_| unavailable("could not close backup file"))?;
    if file.metadata().map(|meta| meta.len()).ok() != Some(bytes) {
        return Err(unavailable(
            "SQLite backup size differs from its pinned source",
        ));
    }
    sync_copy(&file).map_err(|_| unavailable("could not sync backup file"))?;
    Ok(bytes)
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
