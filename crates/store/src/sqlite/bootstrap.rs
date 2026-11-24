//! One-time, stopped-store transition to the encoding-neutral schema.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, OpenFlags, TransactionBehavior};

use super::{
    SCHEMA, SessionStoreError, SqlitePersistenceShape, SqliteSessionStore, admit_readable_records,
    classify_bootstrap_connection, initialize_writer_epoch, map_io, map_sqlite, validate_counts,
    verify_contents, verify_exact_schema, verify_foreign_keys, verify_sqlite_runtime,
};

const LOCK_WAIT: Duration = Duration::from_secs(1);
const LOCK_RETRY: Duration = Duration::from_millis(25);
const REHEARSAL_BYTES: i64 = 16 * 1024 * 1024 * 1024;

impl SqliteSessionStore {
    /// Read-only preflight for the stopped-store transition.
    ///
    /// # Errors
    /// Returns typed shape, integrity, or storage refusals.
    #[doc(hidden)]
    pub fn bootstrap_pending(path: &Path) -> Result<bool, SessionStoreError> {
        Self::preflight_bootstrap(path, super::BUSY_TIMEOUT)
    }

    fn preflight_bootstrap(path: &Path, timeout: Duration) -> Result<bool, SessionStoreError> {
        let Some(reader) = Self::open_inspection_with_timeout(path, timeout)? else {
            return Ok(false);
        };
        let snapshot = reader.unchecked_transaction().map_err(map_sqlite)?;
        let (shape, pending) = classify_bootstrap_connection(&snapshot)?;
        if shape == SqlitePersistenceShape::Current {
            admit_readable_records(&snapshot, pending)?;
        }
        Ok(pending)
    }

    /// Qualify the stopped-store transition on a detached copy of a live source.
    ///
    /// # Errors
    /// Returns typed shape, integrity, or storage refusals.
    #[doc(hidden)]
    pub fn rehearse_bootstrap(source: &Path, scratch: &Path) -> Result<(), SessionStoreError> {
        let reader = Self::open_inspection(source)?.ok_or(SessionStoreError::NotFound)?;
        let snapshot = reader.unchecked_transaction().map_err(map_sqlite)?;
        let bytes: i64 = snapshot
            .query_row(
                "SELECT page_count * page_size FROM pragma_page_count, pragma_page_size",
                [],
                |row| row.get(0),
            )
            .map_err(map_sqlite)?;
        if bytes > REHEARSAL_BYTES {
            return Err(SessionStoreError::BootstrapRehearsalTooLarge);
        }
        copy_into(&snapshot, scratch)?;
        drop(snapshot);
        drop(reader);
        run(scratch, false, |_| {})
    }

    /// Activate the exact new shape, if pending, while the host is stopped.
    ///
    /// # Errors
    /// Refuses if another handle holds the store or verification fails.
    pub fn bootstrap_stopped(path: &Path, cut: impl Fn(&str)) -> Result<(), SessionStoreError> {
        let deadline = Instant::now() + LOCK_WAIT;
        loop {
            match Self::preflight_bootstrap(path, Duration::ZERO) {
                Err(SessionStoreError::BackendBusy) if Instant::now() < deadline => {
                    std::thread::sleep(LOCK_RETRY);
                }
                Err(SessionStoreError::BackendBusy) => {
                    return Err(SessionStoreError::BootstrapBusy);
                }
                Ok(true) => return run_with_deadline(path, true, cut, deadline),
                Ok(false) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }
}

fn run(path: &Path, live: bool, cut: impl Fn(&str)) -> Result<(), SessionStoreError> {
    run_with_deadline(path, live, cut, Instant::now() + LOCK_WAIT)
}

fn run_with_deadline(
    path: &Path,
    live: bool,
    cut: impl Fn(&str),
    deadline: Instant,
) -> Result<(), SessionStoreError> {
    loop {
        match retain_exclusive(path) {
            Err(SessionStoreError::BootstrapBusy) if Instant::now() < deadline => {
                std::thread::sleep(LOCK_RETRY);
            }
            Ok(Some(mut writer)) => {
                if live {
                    backup(&writer, path)?;
                }
                return rebuild(&mut writer, !live, cut);
            }
            Ok(None) => return Ok(()),
            Err(error) => return Err(error),
        }
    }
}

fn retain_exclusive(path: &Path) -> Result<Option<Connection>, SessionStoreError> {
    let mut writer = open_writer(path)?;
    // BEGIN EXCLUSIVE alone is equivalent to IMMEDIATE in WAL. Exclusive
    // locking mode also acquires the main file lock and retains it across the
    // the backup and rebuild, excluding already-open legacy handles.
    writer
        .execute_batch(
            "PRAGMA locking_mode=EXCLUSIVE; PRAGMA trusted_schema=OFF;
         PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;",
        )
        .map_err(bootstrap_lock_error)?;
    let lock = writer
        .transaction_with_behavior(TransactionBehavior::Exclusive)
        .map_err(bootstrap_lock_error)?;
    let (_, pending) = classify_bootstrap_connection(&lock)?;
    if pending {
        admit_readable_records(&lock, true)?;
    }
    lock.commit().map_err(bootstrap_lock_error)?;
    if !pending {
        return Ok(None);
    }
    writer
        .execute_batch("PRAGMA foreign_keys=OFF; PRAGMA legacy_alter_table=ON;")
        .map_err(map_sqlite)?;
    Ok(Some(writer))
}

fn open_writer(path: &Path) -> Result<Connection, SessionStoreError> {
    let writer = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(map_sqlite)?;
    writer.busy_timeout(Duration::ZERO).map_err(map_sqlite)?;
    verify_sqlite_runtime(&writer)?;
    Ok(writer)
}

fn bootstrap_lock_error(error: rusqlite::Error) -> SessionStoreError {
    match map_sqlite(error) {
        SessionStoreError::BackendBusy => SessionStoreError::BootstrapBusy,
        other => other,
    }
}

fn rebuild(
    writer: &mut Connection,
    full_verification: bool,
    cut: impl Fn(&str),
) -> Result<(), SessionStoreError> {
    let tx = writer
        .transaction_with_behavior(TransactionBehavior::Exclusive)
        .map_err(bootstrap_lock_error)?;
    tx.execute_batch("ALTER TABLE runtime_objects RENAME TO runtime_objects_legacy;")
        .map_err(map_sqlite)?;
    tx.execute_batch(table_ddl("runtime_objects")?)
        .map_err(map_sqlite)?;
    tx.execute_batch(
        "INSERT INTO runtime_objects(id,kind,codec,digest,byte_len)
         SELECT id,kind,codec,digest,byte_len FROM runtime_objects_legacy;",
    )
    .map_err(map_sqlite)?;
    let same_rows: bool = tx
        .query_row(
            "SELECT (SELECT count(*) FROM runtime_objects_legacy) =
                    (SELECT count(*) FROM runtime_objects)
                AND NOT EXISTS (
                    SELECT id,kind,codec,digest,byte_len FROM runtime_objects_legacy
                    EXCEPT SELECT id,kind,codec,digest,byte_len FROM runtime_objects
                )",
            [],
            |row| row.get(0),
        )
        .map_err(map_sqlite)?;
    if !same_rows {
        return Err(SessionStoreError::corrupt());
    }
    tx.execute_batch("DROP TABLE runtime_objects_legacy;")
        .map_err(map_sqlite)?;
    tx.execute_batch(table_ddl("runtime_writer")?)
        .map_err(map_sqlite)?;
    initialize_writer_epoch(&tx)?;
    verify_exact_schema(&tx)?;
    validate_counts(&tx)?;
    if full_verification {
        verify_contents(&tx)?;
    } else {
        verify_foreign_keys(&tx)?;
    }
    cut("before-commit");
    tx.commit().map_err(map_sqlite)?;
    cut("after-commit");
    Ok(())
}

fn table_ddl(name: &str) -> Result<&'static str, SessionStoreError> {
    let prefix = format!("CREATE TABLE {name} (");
    SCHEMA
        .split(";\n")
        .map(str::trim_start)
        .find(|ddl| ddl.starts_with(&prefix))
        .ok_or_else(SessionStoreError::corrupt)
}

fn backup(connection: &Connection, database: &Path) -> Result<(), SessionStoreError> {
    let parent = database
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let staging = private_staging(parent)?;
    let pending = staging.join("copy.sqlite3");
    let result = copy_into(connection, &pending).and_then(|()| {
        fs::rename(
            &pending,
            database.with_extension("pre-codec-bootstrap.sqlite3"),
        )
        .map_err(map_io)?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(map_io)
    });
    let _ = fs::remove_file(&pending);
    let _ = fs::remove_dir(&staging);
    result
}

fn private_staging(parent: &Path) -> Result<PathBuf, SessionStoreError> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    loop {
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let staging = parent.join(format!(".runtime-bootstrap-{}-{nonce}", std::process::id()));
        match DirBuilder::new().mode(0o700).create(&staging) {
            Ok(()) => return Ok(staging),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(map_io(error)),
        }
    }
}

fn copy_into(connection: &Connection, path: &Path) -> Result<(), SessionStoreError> {
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(map_io)?;
    let mut copy = open_writer(path)?;
    // The destination is still unpublished scratch; sync the finished file
    // before renaming it instead of syncing during the SQLite copy.
    copy.pragma_update(None, "synchronous", "OFF")
        .map_err(map_sqlite)?;
    match Backup::new(connection, &mut copy)
        .map_err(bootstrap_lock_error)?
        .step(-1)
        .map_err(bootstrap_lock_error)?
    {
        StepResult::Done => {}
        StepResult::Busy | StepResult::Locked => return Err(SessionStoreError::BootstrapBusy),
        _ => return Err(SessionStoreError::BackendUnavailable),
    }
    // Publish a self-contained file that a legacy build can open directly.
    copy.pragma_update(None, "journal_mode", "WAL")
        .map_err(map_sqlite)?;
    copy.close().map_err(|(_, error)| map_sqlite(error))?;
    file.sync_all().map_err(map_io)
}
