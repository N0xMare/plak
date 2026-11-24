//! One-time, stopped-store transition to the encoding-neutral schema.

use std::fs::{self, DirBuilder, File};
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, TransactionBehavior};

use super::{
    SCHEMA, SessionStoreError, SqlitePersistenceShape, SqliteSessionStore, admit_readable_records,
    classify_bootstrap_connection, initialize_writer_epoch, map_io, map_sqlite, validate_counts,
    verify_contents, verify_exact_schema, verify_sqlite_runtime,
};

impl SqliteSessionStore {
    /// Read-only preflight for the stopped-store transition.
    ///
    /// # Errors
    /// Returns typed shape, integrity, or storage refusals.
    #[doc(hidden)]
    pub fn bootstrap_pending(path: &Path) -> Result<bool, SessionStoreError> {
        let Some(reader) = Self::open_inspection(path)? else {
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
        copy_into(&reader, scratch)?;
        let writer = open_writer(scratch)?;
        writer
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(map_sqlite)?;
        drop(writer);
        run(scratch, |_| {})
    }

    /// Activate the exact new shape, if pending, while the host is stopped.
    ///
    /// # Errors
    /// Refuses if another handle holds the store or verification fails.
    #[doc(hidden)]
    pub fn bootstrap_stopped(path: &Path, cut: impl Fn(&str)) -> Result<(), SessionStoreError> {
        if Self::bootstrap_pending(path)? {
            run(path, cut)?;
        }
        Ok(())
    }
}

pub(super) fn run(path: &Path, cut: impl Fn(&str)) -> Result<(), SessionStoreError> {
    let mut writer = open_writer(path)?;
    // BEGIN EXCLUSIVE alone is equivalent to IMMEDIATE in WAL. Exclusive
    // locking mode also acquires the main file lock and retains it across the
    // VACUUM INTO backup and rebuild, excluding already-open legacy handles.
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
        return Ok(());
    }
    backup(&writer, path)?;
    writer
        .execute_batch("PRAGMA foreign_keys=OFF; PRAGMA legacy_alter_table=ON;")
        .map_err(map_sqlite)?;
    rebuild(&mut writer, cut)
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

fn rebuild(writer: &mut Connection, cut: impl Fn(&str)) -> Result<(), SessionStoreError> {
    let tx = writer
        .transaction_with_behavior(TransactionBehavior::Exclusive)
        .map_err(bootstrap_lock_error)?;
    tx.execute_batch("ALTER TABLE runtime_objects RENAME TO runtime_objects_legacy;")
        .map_err(map_sqlite)?;
    tx.execute_batch(table_ddl("runtime_objects")?)
        .map_err(map_sqlite)?;
    tx.execute_batch(
        "INSERT INTO runtime_objects(id,kind,codec,digest,byte_len)
         SELECT id,kind,codec,digest,byte_len FROM runtime_objects_legacy;
         DROP TABLE runtime_objects_legacy;",
    )
    .map_err(map_sqlite)?;
    tx.execute_batch(table_ddl("runtime_writer")?)
        .map_err(map_sqlite)?;
    initialize_writer_epoch(&tx)?;
    verify_exact_schema(&tx)?;
    validate_counts(&tx)?;
    verify_contents(&tx)?;
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
    let name = path.to_str().ok_or(SessionStoreError::BackendUnavailable)?;
    connection
        .execute("VACUUM INTO ?1", [name])
        .map_err(map_sqlite)?;
    let file = File::open(path).map_err(map_io)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .and_then(|()| file.sync_all())
        .map_err(map_io)
}
