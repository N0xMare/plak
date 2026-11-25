//! Single-file SQLite admission shared by the product and tree stores.

mod revision;
pub use revision::{BackedUpUpgrade, LockedUpgrade, copy_snapshot_bounded};

#[cfg(any(test, feature = "test-support"))]
pub(crate) mod test_registry;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};
use sha2::{Digest, Sha256};

/// How long a normal connection waits on another connection's lock.
pub const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

type Objects = &'static [(&'static str, &'static str)];

/// One additive step; the registry position gives its source revision.
pub struct Revision {
    /// Reviewed DDL and deterministic same-store backfill.
    pub sql: &'static str,
    /// Exact resulting DDL for only the new or changed objects.
    pub objects: Objects,
    /// Pinned fingerprint of the complete destination shape.
    pub fingerprint: &'static str,
}

/// One generation's frozen base and its ordered revision registry.
pub struct Schema {
    /// The store's name in messages: `tree` reads "tree store".
    pub noun: &'static str,
    /// The database and identity-table stem: e.g. `subagents`.
    pub stem: &'static str,
    /// What an operator archives before retrying over an unversioned file.
    pub archive: &'static str,
    /// Oldest supported `PRAGMA user_version`, and state-key component.
    pub generation: i64,
    /// Frozen base DDL in creation and fingerprint order.
    pub objects: Objects,
    /// Pinned fingerprint of the base DDL.
    pub base_fingerprint: &'static str,
    /// Steps from the generation to the current revision, without gaps.
    pub revisions: &'static [Revision],
}

/// Whether an admitted store needs an in-place upgrade.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    /// No initialized store.
    Fresh,
    /// The store is at the current revision.
    Current,
    /// The store has a supported older revision.
    Pending(i64),
}

/// Why a file was not admitted.
#[derive(Debug)]
pub enum AdmissionError {
    /// It could not be opened or read exactly.
    Unavailable(String),
    /// It belongs to a schema cohort this build does not read.
    Unsupported(String),
    /// A stopped-store upgrade is required before ordinary opens.
    UpgradePending(String),
    /// The exclusive upgrade lock was unavailable.
    Busy(String),
}

impl std::fmt::Display for AdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(message)
            | Self::Unsupported(message)
            | Self::UpgradePending(message)
            | Self::Busy(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for AdmissionError {}

impl Schema {
    /// The highest supported SQLite user version.
    #[must_use]
    pub const fn version(&self) -> i64 {
        self.generation + self.revisions.len().cast_signed() as i64
    }

    /// The DDL fingerprint this build expects at its current revision.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        self.revisions
            .last()
            .map_or(self.base_fingerprint, |step| step.fingerprint)
            .to_owned()
    }

    /// Verify a read-only snapshot at the current revision.
    pub fn require_current(&self, snapshot: &Connection) -> Result<(), AdmissionError> {
        match self.classify(snapshot)? {
            Status::Current => Ok(()),
            Status::Pending(version) => Err(self.pending_error(version)),
            Status::Fresh => Err(self.unavailable("is not initialized")),
        }
    }

    /// Classify and verify a file without configuring a writer or creating it.
    pub fn inspect(&self, path: &Path) -> Result<Status, AdmissionError> {
        let Some(connection) = self.open_reader(path)? else {
            return Ok(Status::Fresh);
        };
        let snapshot = connection
            .unchecked_transaction()
            .map_err(|_| self.unavailable("inspection could not start"))?;
        self.classify(&snapshot)
    }

    /// Check only the revision, without creating the file or inspecting its shape.
    /// Callers that only fence incompatible builds leave other errors to open.
    pub fn check_revision(&self, path: &Path) -> Result<(), AdmissionError> {
        if let Some(connection) = self.open_reader(path)? {
            self.require_supported(self.read_version(&connection)?)?;
        }
        Ok(())
    }

    fn open_reader(&self, path: &Path) -> Result<Option<Connection>, AdmissionError> {
        match path.symlink_metadata() {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Ok(metadata) if metadata.is_file() => {}
            _ => return Err(self.unavailable("is not an accessible regular file")),
        }
        let connection = Connection::open_with_flags(
            self.resolved_path(path)?,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|_| self.unavailable("could not be opened read-only"))?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(|_| self.unavailable("inspection timeout could not be configured"))?;
        Ok(Some(connection))
    }

    /// Open a normal writer. Only an empty file can change schema here;
    /// existing stores first need stopped-store owner admission.
    pub fn open_writer(
        &self,
        path: &Path,
        install: impl FnOnce(&mut Connection) -> Result<(), AdmissionError>,
    ) -> Result<Connection, AdmissionError> {
        let initial = self.inspect(path)?;
        if let Status::Pending(version) = initial {
            return Err(self.pending_error(version));
        }
        let mut connection = self.writer(path, initial == Status::Fresh)?;
        let status = {
            let snapshot = connection
                .unchecked_transaction()
                .map_err(|_| self.unavailable("admission snapshot could not start"))?;
            self.classify(&snapshot)?
        };
        if let Status::Pending(version) = status {
            return Err(self.pending_error(version));
        }
        self.configure_writer(&connection)?;
        install(&mut connection)?;
        if status == Status::Fresh {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|error| self.sql_error(&error, "initialization could not lock"))?;
            match self.classify(&transaction)? {
                Status::Fresh => self.initialize(&transaction)?,
                Status::Current => {}
                Status::Pending(version) => return Err(self.pending_error(version)),
            }
            transaction
                .commit()
                .map_err(|error| self.sql_error(&error, "initialization could not commit"))?;
        }
        Ok(connection)
    }

    fn writer(&self, path: &Path, create: bool) -> Result<Connection, AdmissionError> {
        let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        flags.set(OpenFlags::SQLITE_OPEN_CREATE, create);
        let connection = Connection::open_with_flags(self.resolved_path(path)?, flags)
            .map_err(|error| self.sql_error(&error, "could not be opened for writing"))?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(|_| self.unavailable("writer timeout could not be configured"))?;
        Ok(connection)
    }

    fn resolved_path(&self, path: &Path) -> Result<PathBuf, AdmissionError> {
        let parent = path
            .parent()
            .ok_or_else(|| self.unavailable("directory is missing"))?;
        let parent = parent
            .canonicalize()
            .map_err(|_| self.unavailable("directory could not be resolved"))?;
        let name = path
            .file_name()
            .ok_or_else(|| self.unavailable("file name is missing"))?;
        Ok(parent.join(name))
    }

    fn configure_writer(&self, connection: &Connection) -> Result<(), AdmissionError> {
        let mode: String = connection
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .map_err(|error| self.sql_error(&error, "could not enable WAL"))?;
        if mode != "wal" {
            return Err(self.unavailable("refused WAL mode"));
        }
        self.configure_durability(connection)
    }

    fn configure_durability(&self, connection: &Connection) -> Result<(), AdmissionError> {
        connection
            .execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")
            .map_err(|_| self.unavailable("could not configure durability"))?;
        let pragma = |name| {
            connection
                .pragma_query_value(None, name, |row| row.get::<_, i64>(0))
                .map_err(|_| self.unavailable("could not read back durability configuration"))
        };
        if pragma("foreign_keys")? != 1 || pragma("synchronous")? != 2 {
            return Err(self.unavailable("did not retain durability configuration"));
        }
        Ok(())
    }

    fn classify(&self, connection: &Connection) -> Result<Status, AdmissionError> {
        let version = self.read_version(connection)?;
        self.require_supported(version)?;
        if version == 0 {
            let has_schema: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%')",
                    [],
                    |row| row.get(0),
                )
                .map_err(|_| self.unavailable("schema could not be inspected"))?;
            if has_schema {
                return Err(AdmissionError::Unavailable(format!(
                    "unversioned {} store contains unrecognized state; archive {}, then retry",
                    self.noun, self.archive
                )));
            }
            return Ok(Status::Fresh);
        }
        self.validate_revision(connection, version)
            .map_err(|error| {
                AdmissionError::Unavailable(format!(
                    "{error}; current-schema {} state is not authoritative",
                    self.noun
                ))
            })?;
        Ok(if version == self.version() {
            Status::Current
        } else {
            Status::Pending(version)
        })
    }

    fn read_version(&self, connection: &Connection) -> Result<i64, AdmissionError> {
        connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|error| self.sql_error(&error, "schema revision could not be inspected"))
    }

    fn require_supported(&self, version: i64) -> Result<(), AdmissionError> {
        let expected = self.version();
        if version == 0 || (self.generation..=expected).contains(&version) {
            return Ok(());
        }
        Err(AdmissionError::Unsupported(format!(
            "{stem} schema {version}, expected {expected}: this {stem}.sqlite3 was created by an incompatible plakd build; move it aside to start fresh",
            stem = self.stem
        )))
    }

    fn initialize(&self, transaction: &Transaction<'_>) -> Result<(), AdmissionError> {
        for (_, sql) in self.objects {
            transaction
                .execute_batch(sql)
                .map_err(|error| self.sql_error(&error, "could not initialize schema"))?;
        }
        transaction
            .execute(
                &format!(
                    "INSERT INTO {}_schema(singleton,instance_id,fingerprint) VALUES(1,?1,?2)",
                    self.stem
                ),
                rusqlite::params![uuid::Uuid::new_v4().to_string(), self.base_fingerprint],
            )
            .map_err(|error| self.sql_error(&error, "could not initialize identity"))?;
        self.apply_steps(transaction, self.generation)
    }

    /// Requires the current shape and its recorded fingerprint.
    pub fn validate(&self, connection: &Connection) -> Result<(), String> {
        self.validate_revision(connection, self.version())
    }

    fn validate_revision(&self, connection: &Connection, version: i64) -> Result<(), String> {
        let mut expected = self.objects.to_vec();
        let mut pin = self.base_fingerprint;
        for (_, step) in self.steps().take_while(|(source, _)| *source < version) {
            for &(name, ddl) in step.objects {
                if let Some((_, previous)) = expected.iter_mut().find(|(key, _)| *key == name) {
                    *previous = ddl;
                } else {
                    expected.push((name, ddl));
                }
            }
            pin = step.fingerprint;
        }
        let actual = self
            .read_shape(connection)
            .map_err(|error| format!("{error} (expected fingerprint {pin})"))?;
        let fingerprint = fingerprint(&expected);
        expected.sort_unstable_by_key(|(name, _)| *name);
        if actual
            .iter()
            .map(|(n, s)| (n.as_str(), s.as_str()))
            .ne(expected)
        {
            return Err(format!(
                "{} store schema shape does not match its exact revision (expected fingerprint {pin})",
                self.noun
            ));
        }
        let stored = self
            .recorded_fingerprint(connection)
            .map_err(|error| format!("{error} (expected fingerprint {pin})"))?;
        if stored != pin || fingerprint != pin {
            return Err(format!(
                "{} store schema fingerprint does not match its exact revision (expected fingerprint {pin})",
                self.noun
            ));
        }
        Ok(())
    }

    fn recorded_fingerprint(&self, connection: &Connection) -> Result<String, String> {
        connection
            .query_row(
                &format!(
                    "SELECT fingerprint FROM {}_schema WHERE singleton=1",
                    self.stem
                ),
                [],
                |row| row.get(0),
            )
            .map_err(|_| format!("{} store schema fingerprint is missing", self.noun))
    }

    fn read_shape(&self, connection: &Connection) -> Result<Vec<(String, String)>, String> {
        let mut statement = connection
            .prepare(
                "SELECT name,sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' AND sql IS NOT NULL ORDER BY name",
            )
            .map_err(|_| format!("could not inspect {} store shape", self.noun))?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .and_then(|rows| rows.collect())
            .map_err(|_| format!("{} store schema shape is invalid", self.noun))
    }

    fn unavailable(&self, detail: &str) -> AdmissionError {
        AdmissionError::Unavailable(format!("{} store {detail}", self.noun))
    }

    fn pending_error(&self, version: i64) -> AdmissionError {
        AdmissionError::UpgradePending(format!(
            "{} store upgrade pending: revision {version} must advance to {} with the old host stopped",
            self.noun,
            self.version()
        ))
    }

    fn sql_error(&self, error: &rusqlite::Error, detail: &str) -> AdmissionError {
        if matches!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
        ) {
            return AdmissionError::Busy(format!("{} store is busy: {detail}", self.noun));
        }
        self.unavailable(detail)
    }
}

fn fingerprint(objects: &[(&str, &str)]) -> String {
    let mut digest = Sha256::new();
    for (name, sql) in objects {
        for part in [name, sql] {
            digest.update((part.len() as u64).to_be_bytes());
            digest.update(part.as_bytes());
        }
    }
    let mut hex = String::with_capacity(64);
    for byte in digest.finalize() {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}
