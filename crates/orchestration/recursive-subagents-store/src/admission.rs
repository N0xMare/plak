//! Single-file `SQLite` admission, shared by this store and the product store.
//!
//! A writer connection runs WAL with `synchronous=FULL` and foreign keys on,
//! and a file is admitted only at its exact schema: an unversioned file with
//! tables in it, another version, or DDL or a fingerprint that differs from
//! the descriptors is refused, never migrated. Each store keeps its own
//! descriptors, version, file and messages; this module only owns the steps.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};

/// How long a writer waits on another connection's lock before failing.
pub const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// One store's schema, as its admission refers to it.
pub struct Schema {
    /// The store's name in messages: `tree` reads "tree store".
    pub noun: &'static str,
    /// The file stem, which also names the schema and its identity table:
    /// `subagents` is `subagents.sqlite3`, "subagents schema 8" and
    /// `subagents_schema`.
    pub stem: &'static str,
    /// What an operator archives before retrying over an unversioned file.
    pub archive: &'static str,
    /// `PRAGMA user_version` of the current cohort.
    pub version: i64,
    /// Every object the file holds, name and exact DDL, in creation order.
    pub objects: &'static [(&'static str, &'static str)],
}

/// Why a file was not admitted.
#[derive(Debug)]
pub enum AdmissionError {
    /// It could not be opened or read exactly.
    Unavailable(String),
    /// It belongs to a schema cohort this build does not read.
    Unsupported(String),
}

impl std::fmt::Display for AdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(message) | Self::Unsupported(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for AdmissionError {}

/// Opens `path` read-write, creating it, without following a final symlink,
/// and configures it as a durable writer: busy timeout, WAL,
/// `foreign_keys=ON` and `synchronous=FULL`, each read back.
pub fn open_writer(path: &Path, noun: &str) -> Result<Connection, String> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|error| format!("could not open {noun} store: {error}"))?;
    connection
        .busy_timeout(BUSY_TIMEOUT)
        .map_err(|_| format!("could not configure {noun} store timeout"))?;
    let mode: String = connection
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .map_err(|_| format!("could not enable {noun} store WAL"))?;
    if mode != "wal" {
        return Err(format!("{noun} store refused WAL mode"));
    }
    connection
        .execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = FULL;")
        .map_err(|_| format!("could not configure {noun} store durability"))?;
    let foreign_keys: i64 = connection
        .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
        .map_err(|_| format!("could not inspect {noun} store"))?;
    let synchronous: i64 = connection
        .query_row("PRAGMA synchronous", [], |row| row.get(0))
        .map_err(|_| format!("could not inspect {noun} store"))?;
    if foreign_keys != 1 || synchronous != 2 {
        return Err(format!(
            "{noun} store durability configuration was not retained"
        ));
    }
    Ok(connection)
}

impl Schema {
    /// This schema's DDL fingerprint: SHA-256 over each object's name and
    /// statement, each prefixed by its big-endian `u64` length, as lowercase
    /// hex.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let mut digest = Sha256::new();
        for (name, sql) in self.objects {
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

    /// Admits the file behind `connection`: creates and versions an empty
    /// one, and otherwise requires this exact version, shape and
    /// fingerprint.
    pub fn admit(&self, connection: &Connection) -> Result<(), AdmissionError> {
        let Self {
            noun,
            stem,
            archive,
            version: expected,
            ..
        } = self;
        let unavailable = |message: String| AdmissionError::Unavailable(message);
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|_| unavailable(format!("could not inspect {noun} store schema")))?;
        let has_schema: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%')",
                [],
                |row| row.get(0),
            )
            .map_err(|_| unavailable(format!("could not inspect {noun} store schema")))?;
        if version == 0 && has_schema {
            return Err(unavailable(format!(
                "unversioned {noun} store contains unrecognized state; archive {archive}, then retry"
            )));
        }
        if version == 0 {
            return self.initialize(connection).map_err(unavailable);
        }
        if version != *expected {
            return Err(AdmissionError::Unsupported(format!(
                "{stem} schema {version}, expected {expected}: this {stem}.sqlite3 was created by an incompatible plak build; move it aside to start fresh"
            )));
        }
        self.validate(connection).map_err(|error| {
            unavailable(format!(
                "{error}; current-schema {noun} state is not authoritative"
            ))
        })
    }

    fn initialize(&self, connection: &Connection) -> Result<(), String> {
        let Self {
            noun,
            stem,
            version,
            objects,
            ..
        } = self;
        let transaction = connection
            .unchecked_transaction()
            .map_err(|_| format!("could not initialize {noun} store"))?;
        for (_, sql) in *objects {
            transaction
                .execute_batch(sql)
                .map_err(|_| format!("could not initialize {noun} store schema"))?;
        }
        let changed = transaction
            .execute(
                &format!(
                    "INSERT INTO {stem}_schema(singleton,instance_id,fingerprint) VALUES(1,?1,?2)"
                ),
                rusqlite::params![uuid::Uuid::new_v4().to_string(), self.fingerprint()],
            )
            .map_err(|_| format!("could not initialize {noun} store identity"))?;
        if changed != 1 {
            return Err(format!("could not initialize exact {noun} store identity"));
        }
        transaction
            .pragma_update(None, "user_version", version)
            .map_err(|_| format!("could not version {noun} store schema"))?;
        transaction
            .commit()
            .map_err(|_| format!("could not commit {noun} store schema"))
    }

    /// Requires the file's objects to be exactly these descriptors and its
    /// recorded fingerprint to be theirs. Ordinary open does not scan page
    /// integrity or foreign keys.
    pub fn validate(&self, connection: &Connection) -> Result<(), String> {
        let Self {
            noun,
            stem,
            objects,
            ..
        } = self;
        let mut statement = connection
            .prepare(
                "SELECT name,sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' AND sql IS NOT NULL ORDER BY name",
            )
            .map_err(|_| format!("could not inspect {noun} store shape"))?;
        let actual = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|_| format!("could not inspect {noun} store shape"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| format!("{noun} store shape is invalid"))?;
        let mut expected = objects
            .iter()
            .map(|(name, sql)| ((*name).to_owned(), (*sql).to_owned()))
            .collect::<Vec<_>>();
        expected.sort_by(|left, right| left.0.cmp(&right.0));
        if actual != expected {
            return Err(format!(
                "{noun} store schema shape does not match its exact revision"
            ));
        }
        let fingerprint: String = connection
            .query_row(
                &format!("SELECT fingerprint FROM {stem}_schema WHERE singleton=1"),
                [],
                |row| row.get(0),
            )
            .map_err(|_| format!("{noun} store schema fingerprint is missing"))?;
        if fingerprint != self.fingerprint() {
            return Err(format!(
                "{noun} store schema fingerprint does not match its exact revision"
            ));
        }
        Ok(())
    }
}
