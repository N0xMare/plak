//! One-time, stopped-store revisions.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, OpenFlags, TransactionBehavior};

use super::{
    ADMISSION_REVISION, AdmissionRevision, SCHEMA, SessionStoreError, SqliteSessionStore,
    admit_readable_records, classify_bootstrap_connection, initialize_writer_epoch, map_io,
    map_sqlite, validate_admission_counts, verify_contents, verify_exact_schema,
    verify_foreign_keys, verify_sqlite_runtime,
};

const LOCK_WAIT: Duration = Duration::from_secs(1);
const LOCK_RETRY: Duration = Duration::from_millis(25);

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
        let (_, revision) = classify_bootstrap_connection(&snapshot)?;
        Ok(revision != AdmissionRevision::Indexed)
    }

    /// Apply bootstrap to a private copy, bounded by the supplied deadline.
    #[doc(hidden)]
    pub fn rehearse_copied_bootstrap(
        path: &Path,
        deadline: Instant,
    ) -> Result<(), SessionStoreError> {
        run_with_deadline(path, false, |_| {}, deadline, Some(deadline))
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
                Ok(true) => return run_with_deadline(path, true, cut, deadline, None),
                Ok(false) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }
}

fn run_with_deadline(
    path: &Path,
    live: bool,
    cut: impl Fn(&str),
    deadline: Instant,
    work_deadline: Option<Instant>,
) -> Result<(), SessionStoreError> {
    loop {
        match retain_exclusive(path, live) {
            Err(SessionStoreError::BootstrapBusy) if Instant::now() < deadline => {
                std::thread::sleep(LOCK_RETRY);
            }
            Ok(Some((mut writer, revision))) => {
                if live {
                    backup(&writer, path, revision)?;
                } else if let Some(deadline) = work_deadline {
                    writer
                        .progress_handler(4096, Some(move || Instant::now() >= deadline))
                        .map_err(map_sqlite)?;
                }
                return migrate(&mut writer, revision, !live, cut);
            }
            Ok(None) => return Ok(()),
            Err(error) => return Err(error),
        }
    }
}

fn retain_exclusive(
    path: &Path,
    live: bool,
) -> Result<Option<(Connection, AdmissionRevision)>, SessionStoreError> {
    let mut writer = open_writer(path)?;
    if !live {
        // A detached SQLite backup may not inherit the source's journal mode.
        writer
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(bootstrap_lock_error)?;
    }
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
    let (_, revision) = classify_bootstrap_connection(&lock)?;
    lock.commit().map_err(bootstrap_lock_error)?;
    if revision == AdmissionRevision::Indexed {
        return Ok(None);
    }
    if revision == AdmissionRevision::Legacy {
        writer
            .execute_batch("PRAGMA foreign_keys=OFF; PRAGMA legacy_alter_table=ON;")
            .map_err(map_sqlite)?;
    }
    Ok(Some((writer, revision)))
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

fn migrate(
    writer: &mut Connection,
    revision: AdmissionRevision,
    full_verification: bool,
    cut: impl Fn(&str),
) -> Result<(), SessionStoreError> {
    let tx = writer
        .transaction_with_behavior(TransactionBehavior::Exclusive)
        .map_err(bootstrap_lock_error)?;
    if revision == AdmissionRevision::Legacy {
        rebuild_objects(&tx)?;
    }
    tx.execute_batch(ADMISSION_REVISION).map_err(map_sqlite)?;
    verify_exact_schema(&tx)?;
    admit_readable_records(&tx, AdmissionRevision::Indexed)?;
    validate_admission_counts(&tx)?;
    match (revision, full_verification) {
        (AdmissionRevision::Legacy, true) => verify_contents(&tx)?,
        (AdmissionRevision::Legacy, false) => verify_foreign_keys(&tx)?,
        _ => {}
    }
    cut("before-commit");
    tx.commit().map_err(map_sqlite)?;
    cut("after-commit");
    Ok(())
}

fn rebuild_objects(tx: &rusqlite::Transaction<'_>) -> Result<(), SessionStoreError> {
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
    initialize_writer_epoch(tx)
}

fn table_ddl(name: &str) -> Result<&'static str, SessionStoreError> {
    let prefix = format!("CREATE TABLE {name} (");
    SCHEMA
        .split(";\n")
        .map(str::trim_start)
        .find(|ddl| ddl.starts_with(&prefix))
        .ok_or_else(SessionStoreError::corrupt)
}

fn backup(
    connection: &Connection,
    database: &Path,
    revision: AdmissionRevision,
) -> Result<(), SessionStoreError> {
    let parent = database
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let staging = private_staging(parent)?;
    let pending = staging.join("copy.sqlite3");
    let result = copy_into(connection, &pending).and_then(|()| {
        let stem = if revision == AdmissionRevision::Legacy {
            "pre-codec-bootstrap"
        } else {
            "pre-admission-revision"
        };
        publish_copy(&pending, database, stem)?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(map_io)
    });
    let _ = fs::remove_file(&pending);
    let _ = fs::remove_dir(&staging);
    result
}

fn publish_copy(pending: &Path, database: &Path, stem: &str) -> Result<(), SessionStoreError> {
    // A crash before migration commits can leave a published copy. It may no
    // longer match the writable source, so publish a fresh copy without replacing it.
    for retry in 0..=u64::MAX {
        let extension = if retry == 0 {
            format!("{stem}.sqlite3")
        } else {
            format!("{stem}-{retry}.sqlite3")
        };
        match fs::hard_link(pending, database.with_extension(extension)) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(map_io(error)),
        }
    }
    Err(SessionStoreError::BackendUnavailable)
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
