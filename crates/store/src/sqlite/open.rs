//! Startup admission, header checks, and private-file preparation.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the sqlite module that shares its vocabulary"
)]

use super::*;

pub(super) const BUSY_TIMEOUT: Duration = Duration::from_secs(15);

pub(super) const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StartupStage {
    Header,
    PrivateFile,
    Actor,
    Connection,
    Runtime,
    Configuration,
    Schema,
    Admission,
    Ready,
}

impl StartupStage {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Header => "header",
            Self::PrivateFile => "private_file",
            Self::Actor => "actor",
            Self::Connection => "connection",
            Self::Runtime => "runtime",
            Self::Configuration => "configuration",
            Self::Schema => "schema",
            Self::Admission => "admission",
            Self::Ready => "ready",
        }
    }
}

pub(super) struct StartupProgress(pub(super) Mutex<StartupStage>);

impl StartupProgress {
    pub(super) fn set(&self, stage: StartupStage) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = stage;
    }

    pub(super) fn failure(
        &self,
        category: &'static str,
        error: SessionStoreError,
    ) -> StartupFailure {
        StartupFailure {
            stage: *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            category,
            error,
        }
    }
}

#[derive(Debug)]
pub(super) struct StartupFailure {
    pub(super) stage: StartupStage,
    pub(super) category: &'static str,
    pub(super) error: SessionStoreError,
}

impl StartupFailure {
    pub(super) fn report(self) -> SessionStoreError {
        // Static categories only: never format a database path, SQL, or backend source.
        tracing::warn!(
            target: "plakous_store::sqlite_startup",
            stage = self.stage.label(),
            category = self.category,
            store_error = startup_error_category(self.error),
            "SQLite store startup failed"
        );
        self.error
    }
}

/// Admits identity, schema version, shape and counters inside one read-only
/// inspection snapshot.
/// Bounded shape of one read-only connection: fresh, or exactly this build's
/// application, version, shape pragmas and schema.
pub(super) fn classify_connection(
    c: &Connection,
) -> Result<SqlitePersistenceShape, SessionStoreError> {
    let app: i64 = c
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(map_sqlite)?;
    let version: i64 = c
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(map_sqlite)?;
    let objects: i64 = c
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )
        .map_err(map_sqlite)?;
    if app == 0 && version == 0 && objects == 0 {
        return Ok(SqlitePersistenceShape::MissingOrFresh);
    }
    if app != APPLICATION_ID {
        return Err(SessionStoreError::corrupt());
    }
    if version != SCHEMA_VERSION {
        return Err(SessionStoreError::UnsupportedSchema);
    }
    verify_shape_pragmas(c)?;
    verify_exact_schema(c)?;
    Ok(SqlitePersistenceShape::Current)
}

/// Admits one inspection snapshot: a current shape and a bound clock domain.
///
/// Retained-row counts are not revalidated. They guard the writer's admission
/// accounting, which inspection never touches, and counting them is O(store)
/// on every read. A fresh file has nothing to inspect.
pub(super) fn admit_inspection_snapshot(snapshot: &Connection) -> Result<(), SessionStoreError> {
    if classify_connection(snapshot)? != SqlitePersistenceShape::Current {
        return Err(SessionStoreError::NotFound);
    }
    bound_domain(snapshot)?;
    Ok(())
}

/// The file half of the header preflight: refuses a non-regular file without
/// blocking on it and a foreign or unsupported main-file header. Returns whether
/// the file has content whose committed identity still needs a connection.
pub(super) fn admit_header_file(path: &Path) -> Result<bool, SessionStoreError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // Reject non-regular descriptors below without blocking on a FIFO open,
        // including a FIFO swapped into the path immediately before this call.
        options.custom_flags(
            (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK)
                .bits()
                .cast_signed(),
        );
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(map_io(error)),
    };
    let metadata = file.metadata().map_err(map_io)?;
    if !metadata.is_file() {
        return Err(SessionStoreError::BackendUnavailable);
    }
    if metadata.len() == 0 {
        return Ok(false);
    }
    let mut header = [0_u8; 72];
    file.read_exact(&mut header)
        .map_err(|_| SessionStoreError::corrupt())?;
    if &header[..16] != b"SQLite format 3\0" {
        return Err(SessionStoreError::corrupt());
    }
    let version = i64::from(u32::from_be_bytes(
        header[60..64]
            .try_into()
            .map_err(|_| SessionStoreError::corrupt())?,
    ));
    let application = i64::from(u32::from_be_bytes(
        header[68..72]
            .try_into()
            .map_err(|_| SessionStoreError::corrupt())?,
    ));
    if application != 0 || version != 0 {
        if application != APPLICATION_ID {
            return Err(SessionStoreError::corrupt());
        }
        if version != SCHEMA_VERSION {
            return Err(SessionStoreError::UnsupportedSchema);
        }
    }
    Ok(true)
}

/// Refuse unsupported versions before opening a writer or adjusting permissions.
/// Read-only WAL inspection may reconstruct or update temporary SHM coordination,
/// but preserves the database and WAL bytes. A supplied clock domain also
/// refuses a foreign same-boot scope before any writer can alter journal state.
/// The authoritative transaction repeats the domain check after this preflight.
pub(super) fn admit_existing_header(
    path: &Path,
    domain: Option<LeaseDomain>,
) -> Result<(), SessionStoreError> {
    if !admit_header_file(path)? {
        return Ok(());
    }
    // A WAL can carry a newer header than the main file. Read its committed
    // identity without opening a writer or checkpointing it into the main file.
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(map_sqlite)?;
    let application: i64 = connection
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(map_sqlite)?;
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(map_sqlite)?;
    if application == 0 && version == 0 {
        let objects: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )
            .map_err(map_sqlite)?;
        return if objects == 0 {
            Ok(())
        } else {
            Err(SessionStoreError::corrupt())
        };
    }
    if application != APPLICATION_ID {
        return Err(SessionStoreError::corrupt());
    }
    if version != SCHEMA_VERSION {
        return Err(SessionStoreError::UnsupportedSchema);
    }
    if let Some(domain) = domain {
        connection
            .execute_batch("PRAGMA trusted_schema=OFF;")
            .map_err(map_sqlite)?;
        verify_exact_schema(&connection)?;
        let bound = bound_domain(&connection)?;
        if bound.epoch() == domain.epoch() && bound != domain {
            return Err(SessionStoreError::LeaseDomainMismatch);
        }
    }
    Ok(())
}

pub(super) fn wait_for_actor(
    ready: &mpsc::Receiver<Result<(), SessionStoreError>>,
    timeout: Duration,
    progress: &StartupProgress,
) -> Result<(), StartupFailure> {
    match ready.recv_timeout(timeout) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(progress.failure("backend", error)),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            Err(progress.failure("readiness_timeout", SessionStoreError::BackendUnavailable))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(progress.failure(
            "readiness_disconnected",
            SessionStoreError::BackendUnavailable,
        )),
    }
}

pub(super) fn wait_for_actor_or_join<S>(
    ready: mpsc::Receiver<Result<(), SessionStoreError>>,
    sender: S,
    join: JoinHandle<()>,
    timeout: Duration,
    progress: &StartupProgress,
) -> Result<JoinHandle<()>, SessionStoreError> {
    match wait_for_actor(&ready, timeout, progress) {
        Ok(()) => Ok(join),
        Err(failure) => {
            // Report the snapshot before joining: late actor progress cannot
            // overwrite the failing stage, and a stalled join stays diagnosable.
            let error = failure.report();
            drop(ready);
            drop(sender);
            let _ = join.join();
            Err(error)
        }
    }
}

pub(super) fn prepare_private_file(path: &Path) -> Result<(), SessionStoreError> {
    match path.symlink_metadata() {
        Ok(_) => {
            let mut existing = OpenOptions::new();
            existing.read(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                existing.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed());
            }
            let file = existing.open(path).map_err(map_io)?;
            return prepare_opened_private_file(&file);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(map_io(error)),
    }
    let mut o = OpenOptions::new();
    // `create_new` makes creation atomic and refuses a concurrently inserted
    // symlink. Existing files are never chmod'd by path: SQLite's subsequent
    // authoritative open uses SQLITE_OPEN_NOFOLLOW and verifies the schema.
    o.create_new(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        o.mode(0o600);
    }
    let file = match o.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut existing = OpenOptions::new();
            existing.read(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                existing.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed());
            }
            let file = existing.open(path).map_err(map_io)?;
            return prepare_opened_private_file(&file);
        }
        Err(error) => return Err(map_io(error)),
    };
    prepare_opened_private_file(&file)
}

fn prepare_opened_private_file(file: &File) -> Result<(), SessionStoreError> {
    let metadata = file.metadata().map_err(map_io)?;
    if !metadata.file_type().is_file() {
        return Err(SessionStoreError::BackendUnavailable);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(map_io)?;
    }
    let metadata = file.metadata().map_err(map_io)?;
    require_private_regular_file(&metadata)
}

pub(super) fn require_private_regular_file(
    metadata: &fs::Metadata,
) -> Result<(), SessionStoreError> {
    if !metadata.file_type().is_file() {
        return Err(SessionStoreError::BackendUnavailable);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(SessionStoreError::BackendUnavailable);
        }
    }
    Ok(())
}
