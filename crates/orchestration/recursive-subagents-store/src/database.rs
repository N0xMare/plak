//! The tree's own database: `subagents.sqlite3` (spec 03 §14).
//!
//! One file, one schema version, one DDL fingerprint, and the same
//! fail-closed law the product store keeps: an unversioned file with tables
//! in it, a version that is not this one, a shape that does not match the
//! descriptors, or a fingerprint that does not match them is refused, never
//! migrated. A schema change moves the product's state directory (spec 02
//! §2.5), so a build never finds another build's file in its own.

use std::path::{Path, PathBuf};

use plakous_recursive_subagents::tree::PersistedTree;
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};

use crate::rows::load_agent_tree;

/// `subagents.sqlite3` starts at schema version 1. It is not the
/// product schema's number and never follows it; both name the state
/// directory.
pub const SCHEMA_VERSION: i64 = 8;

const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Every object the tree's database holds, name and exact DDL. The
/// fingerprint is taken over this list, so a changed byte here needs a new
/// schema version, which moves the state directory.
pub const SCHEMA_OBJECTS: &[(&str, &str)] = &[
    (
        "subagents_schema",
        "CREATE TABLE subagents_schema(singleton INTEGER PRIMARY KEY CHECK(singleton=1),instance_id TEXT NOT NULL UNIQUE,fingerprint TEXT NOT NULL)",
    ),
    (
        "agent_trees",
        "CREATE TABLE agent_trees(tree_id TEXT PRIMARY KEY,root_agent_id TEXT NOT NULL,revision INTEGER NOT NULL CHECK(revision>0),host_generation INTEGER NOT NULL CHECK(host_generation>0),state TEXT NOT NULL CHECK(state IN('live','interrupted','restored_paused','tree_terminal','closed')),writer_mode TEXT NOT NULL CHECK(writer_mode IN('single_lease')),selected_tuple TEXT NOT NULL,launch_checkout TEXT NOT NULL,snapshot_json TEXT NOT NULL)",
    ),
    (
        "agent_nodes",
        "CREATE TABLE agent_nodes(tree_id TEXT NOT NULL,agent_id TEXT NOT NULL,parent_agent_id TEXT,depth INTEGER NOT NULL CHECK(depth>=0 AND depth<=4),description TEXT NOT NULL,prompt TEXT NOT NULL,output_schema_json TEXT NOT NULL,schema_digest TEXT NOT NULL,template TEXT,tuple_id TEXT NOT NULL,account TEXT NOT NULL,effort TEXT,status TEXT NOT NULL CHECK(status IN('pending','running','paused','sealing','completed','interrupted','failed','closed','recovery_required')),workspace_id TEXT NOT NULL,tier TEXT NOT NULL CHECK(tier IN('read_only','execute','mutate')),attempt_id TEXT,session_locator TEXT,path TEXT NOT NULL,branch TEXT,active_turn INTEGER NOT NULL CHECK(active_turn IN(0,1)),fold_order INTEGER CHECK(fold_order>=0),PRIMARY KEY(tree_id,agent_id),FOREIGN KEY(tree_id) REFERENCES agent_trees(tree_id))",
    ),
    (
        "agent_grants",
        "CREATE TABLE agent_grants(tree_id TEXT NOT NULL,agent_id TEXT NOT NULL,may_spawn INTEGER NOT NULL CHECK(may_spawn IN(0,1)),may_adopt INTEGER NOT NULL CHECK(may_adopt IN(0,1)),max_depth INTEGER NOT NULL CHECK(max_depth>=0 AND max_depth<=4),max_nodes INTEGER NOT NULL CHECK(max_nodes>=1 AND max_nodes<=65),max_children INTEGER NOT NULL CHECK(max_children>=1 AND max_children<=64),visible_ids TEXT NOT NULL,message_peers TEXT NOT NULL,managed_ids TEXT NOT NULL,interrupt_ids TEXT NOT NULL,close_ids TEXT NOT NULL,PRIMARY KEY(tree_id,agent_id),FOREIGN KEY(tree_id,agent_id) REFERENCES agent_nodes(tree_id,agent_id))",
    ),
    (
        "agent_operations",
        "CREATE TABLE agent_operations(tree_id TEXT NOT NULL,operation_id TEXT NOT NULL,caller_agent_id TEXT NOT NULL,tool_name TEXT NOT NULL,canonical_request TEXT NOT NULL,request_digest TEXT NOT NULL,terminal_code TEXT,terminal_response TEXT,fold_anchor INTEGER CHECK(fold_anchor>=0),fold_order INTEGER CHECK(fold_order>=1),CHECK((fold_anchor IS NULL)=(fold_order IS NULL)),PRIMARY KEY(tree_id,operation_id),FOREIGN KEY(tree_id) REFERENCES agent_trees(tree_id))",
    ),
    (
        "agent_messages",
        "CREATE TABLE agent_messages(tree_id TEXT NOT NULL,message_id TEXT NOT NULL,from_agent TEXT NOT NULL,to_agent TEXT NOT NULL,body TEXT NOT NULL,purpose TEXT NOT NULL CHECK(purpose IN('coordinate','finding','question','reply','followup','result')),priority TEXT NOT NULL CHECK(priority IN('deferred','urgent')),in_reply_to TEXT,disposition TEXT NOT NULL CHECK(disposition IN('started','queued','steered')),durable TEXT NOT NULL CHECK(durable IN('accepted','acknowledged','delivered','terminal')),PRIMARY KEY(tree_id,message_id),FOREIGN KEY(tree_id) REFERENCES agent_trees(tree_id))",
    ),
    (
        "agent_attempts",
        "CREATE TABLE agent_attempts(tree_id TEXT NOT NULL,attempt_id TEXT NOT NULL,agent_id TEXT NOT NULL,status TEXT NOT NULL CHECK(status IN('running','submitted','completed','completed_without_submission','failed','cancelled','recovery_required','closed','blocked','adopted')),turn_token INTEGER NOT NULL CHECK(turn_token>=1),turn_id TEXT,output_json TEXT,submit_harness_nudge INTEGER NOT NULL CHECK(submit_harness_nudge>=0 AND submit_harness_nudge<=2),candidate_id TEXT,retry_of TEXT,request_key TEXT,started_at INTEGER NOT NULL CHECK(started_at>=0),ended_at INTEGER CHECK(ended_at IS NULL OR ended_at>=0),blocked_kind TEXT,blocked_account TEXT,blocked_resets_at INTEGER,failure_kind TEXT,failure_message TEXT,elapsed_ms INTEGER NOT NULL CHECK(elapsed_ms>=0),turns INTEGER NOT NULL CHECK(turns>=0),tool_calls INTEGER NOT NULL CHECK(tool_calls>=0),input_tokens INTEGER NOT NULL CHECK(input_tokens>=0),output_tokens INTEGER NOT NULL CHECK(output_tokens>=0),spawn_identity TEXT CHECK((request_key IS NULL) = (spawn_identity IS NULL)),output_schema_json TEXT NOT NULL,schema_digest TEXT NOT NULL,applied_terminal_read_seqs TEXT NOT NULL CHECK(json_valid(applied_terminal_read_seqs) AND json_type(applied_terminal_read_seqs)='array'),PRIMARY KEY(tree_id,attempt_id),FOREIGN KEY(tree_id,agent_id) REFERENCES agent_nodes(tree_id,agent_id))",
    ),
    (
        "agent_attempts_request_key",
        "CREATE UNIQUE INDEX agent_attempts_request_key ON agent_attempts(tree_id,request_key) WHERE request_key IS NOT NULL",
    ),
    (
        "agent_candidates",
        "CREATE TABLE agent_candidates(tree_id TEXT NOT NULL,candidate_id TEXT NOT NULL,workspace_id TEXT NOT NULL,producing_agent_id TEXT NOT NULL,producing_attempt_id TEXT NOT NULL,base_commit TEXT NOT NULL,base_content TEXT NOT NULL,base_change_id TEXT,commit_id TEXT NOT NULL,content TEXT NOT NULL,change_id TEXT,sealed_at INTEGER NOT NULL CHECK(sealed_at>=0),adopted_by TEXT,adopted_evidence TEXT NOT NULL,merged_at TEXT,PRIMARY KEY(tree_id,candidate_id),FOREIGN KEY(tree_id) REFERENCES agent_trees(tree_id))",
    ),
    (
        "agent_workspaces",
        "CREATE TABLE agent_workspaces(tree_id TEXT NOT NULL,workspace_id TEXT NOT NULL,owner_agent_id TEXT NOT NULL,path TEXT NOT NULL,mode TEXT NOT NULL CHECK(mode IN('inherit','snapshot','fork')),status TEXT NOT NULL CHECK(status IN('active','adopting','discarding','clean_removed','adopted','discarded')),base_commit TEXT,base_content TEXT,base_change_id TEXT,generation INTEGER NOT NULL CHECK(generation>=0),adopter_agent_id TEXT CHECK(adopter_agent_id IS NULL OR status='adopting'),PRIMARY KEY(tree_id,workspace_id),FOREIGN KEY(tree_id) REFERENCES agent_trees(tree_id))",
    ),
    (
        "agent_commands",
        "CREATE TABLE agent_commands(tree_id TEXT NOT NULL,command_id TEXT NOT NULL,recipient_agent_id TEXT NOT NULL,attempt_id TEXT,kind TEXT NOT NULL CHECK(kind IN('root_batch','start','wake','mail_start','followup','corrective','resume')),canonical_text TEXT NOT NULL,message_ids TEXT NOT NULL,withdrawn INTEGER NOT NULL CHECK(withdrawn IN(0,1)),fact TEXT CHECK(fact IS NULL OR fact IN('accepted','rejected','returned')),turn_id TEXT CHECK(turn_id IS NULL OR fact IS NOT NULL),accepted_seq INTEGER CHECK(accepted_seq IS NULL OR accepted_seq>=1),PRIMARY KEY(tree_id,command_id),FOREIGN KEY(tree_id) REFERENCES agent_trees(tree_id))",
    ),
    (
        "agent_feed_cursors",
        "CREATE TABLE agent_feed_cursors(tree_id TEXT NOT NULL,runtime_session_id TEXT NOT NULL,applied_seq INTEGER NOT NULL CHECK(applied_seq>=0),PRIMARY KEY(tree_id,runtime_session_id),FOREIGN KEY(tree_id) REFERENCES agent_trees(tree_id))",
    ),
    (
        "agent_nodes_tree",
        "CREATE INDEX agent_nodes_tree ON agent_nodes(tree_id,parent_agent_id,agent_id)",
    ),
];

/// The DDL fingerprint this cohort pins.
#[must_use]
pub fn expected_schema_fingerprint() -> String {
    let mut digest = Sha256::new();
    for (name, sql) in SCHEMA_OBJECTS {
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

/// Why the tree database could not be admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenErrorKind {
    /// The file belongs to a schema cohort this build does not read.
    UnsupportedPersistence,
    /// It could not be opened exactly, for any other reason.
    Unavailable,
}

/// Exact read-only tree inspection result. Neither absence class proves that
/// an earlier child effect or external process did not run.
#[derive(Debug)]
pub enum TreeInspection {
    /// No tree database file exists at the selected home.
    StoreAbsent,
    /// The admitted tree database contains no tree with this identity.
    TreeAbsent,
    /// Exact persisted rows, without applying the in-memory reopen law.
    Tree(Box<PersistedTree>),
}

/// Redaction-safe admission failure.
#[derive(Debug)]
pub struct OpenError {
    kind: OpenErrorKind,
    message: String,
}

impl OpenError {
    /// An unavailable failure the caller already has as text.
    #[must_use]
    pub fn unavailable_text(message: impl Into<String>) -> Self {
        Self::unavailable(message)
    }

    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: OpenErrorKind::Unavailable,
            message: message.into(),
        }
    }

    fn unsupported(message: impl Into<String>) -> Self {
        Self {
            kind: OpenErrorKind::UnsupportedPersistence,
            message: message.into(),
        }
    }

    /// Which admission law refused the file, so a caller can tell a
    /// retirable schema mismatch from an unreadable file.
    #[must_use]
    pub const fn kind(&self) -> OpenErrorKind {
        self.kind
    }

    /// The operator-facing sentence, already naming the file and the exact
    /// mismatch; it is the refusal the doctor prints verbatim.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for OpenError {}

/// One connection to the tree's database, held for the life of the runtime
/// that owns it.
pub struct SubagentsDatabase {
    pub(crate) connection: Connection,
}

impl SubagentsDatabase {
    /// Reads an existing tree on one read-only snapshot without opening a
    /// writer, creating directories, or applying the tree reopen law.
    pub(crate) fn inspect_tree(
        database: &Path,
        tree_id: &str,
    ) -> Result<TreeInspection, OpenError> {
        let parent = database
            .parent()
            .ok_or_else(|| OpenError::unavailable("tree store path has no parent"))?;
        let parent = match std::fs::canonicalize(parent) {
            Ok(parent) => parent,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(TreeInspection::StoreAbsent);
            }
            Err(_) => {
                return Err(OpenError::unavailable(
                    "tree store directory is unavailable",
                ));
            }
        };
        let path = parent.join(
            database
                .file_name()
                .ok_or_else(|| OpenError::unavailable("tree store file name is missing"))?,
        );
        let metadata = match path.symlink_metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(TreeInspection::StoreAbsent);
            }
            Err(_) => {
                return Err(OpenError::unavailable(
                    "tree store is unavailable for inspection",
                ));
            }
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(OpenError::unavailable(
                "tree store is not a regular file for inspection",
            ));
        }
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|_| OpenError::unavailable("could not open tree store for inspection"))?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(|_| OpenError::unavailable("could not configure tree inspection timeout"))?;
        connection
            .execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(|_| OpenError::unavailable("could not configure tree inspection"))?;
        let store = Self { connection };
        let tx = store
            .connection
            .unchecked_transaction()
            .map_err(|_| OpenError::unavailable("could not start tree inspection"))?;
        let version: i64 = tx
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|_| OpenError::unavailable("could not inspect tree schema"))?;
        if version != SCHEMA_VERSION {
            return Err(OpenError::unsupported(format!(
                "subagents schema {version}, expected {SCHEMA_VERSION}: created by an incompatible plak build"
            )));
        }
        store.validate_shape().map_err(|error| {
            OpenError::unavailable(format!("{error}; tree inspection has no authority"))
        })?;
        load_agent_tree(&tx, tree_id)
            .map(|tree| {
                tree.map_or(TreeInspection::TreeAbsent, |tree| {
                    TreeInspection::Tree(Box::new(tree))
                })
            })
            .map_err(OpenError::unavailable)
    }

    /// Opens `database`, creating and versioning it when it is new and
    /// refusing it when it belongs to any other cohort.
    pub fn open(database: &Path) -> Result<Self, OpenError> {
        let database = resolved_path(database)?;
        let connection = Connection::open_with_flags(
            &database,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|error| OpenError::unavailable(format!("could not open tree store: {error}")))?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(|_| OpenError::unavailable("could not configure tree store timeout"))?;
        let mode: String = connection
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .map_err(|_| OpenError::unavailable("could not enable tree store WAL"))?;
        if mode != "wal" {
            return Err(OpenError::unavailable("tree store refused WAL mode"));
        }
        connection
            .execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = FULL;")
            .map_err(|_| OpenError::unavailable("could not configure tree store durability"))?;
        let foreign_keys: i64 = connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .map_err(|_| OpenError::unavailable("could not inspect tree store"))?;
        let synchronous: i64 = connection
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .map_err(|_| OpenError::unavailable("could not inspect tree store"))?;
        if foreign_keys != 1 || synchronous != 2 {
            return Err(OpenError::unavailable(
                "tree store durability configuration was not retained",
            ));
        }
        #[cfg(feature = "commit-trace")]
        plakous_store::commit_trace::install(&connection, "tree")
            .map_err(|_| OpenError::unavailable("could not install commit trace"))?;
        let store = Self { connection };
        store.admit_schema()?;
        Ok(store)
    }

    fn admit_schema(&self) -> Result<(), OpenError> {
        let version: i64 = self
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|_| OpenError::unavailable("could not inspect tree store schema"))?;
        let has_schema: bool = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%')",
                [],
                |row| row.get(0),
            )
            .map_err(|_| OpenError::unavailable("could not inspect tree store schema"))?;
        if version == 0 && has_schema {
            return Err(OpenError::unavailable(
                "unversioned tree store contains unrecognized state; archive subagents.sqlite3, then retry",
            ));
        }
        if version == 0 {
            return self.initialize();
        }
        if version != SCHEMA_VERSION {
            return Err(OpenError::unsupported(format!(
                "subagents schema {version}, expected {SCHEMA_VERSION}: this subagents.sqlite3 was created by an incompatible plak build; move it aside to start fresh"
            )));
        }
        self.validate_shape().map_err(|error| {
            OpenError::unavailable(format!(
                "{error}; current-schema tree state is not authoritative"
            ))
        })
    }

    fn initialize(&self) -> Result<(), OpenError> {
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(|_| OpenError::unavailable("could not initialize tree store"))?;
        for (_, sql) in SCHEMA_OBJECTS {
            transaction
                .execute_batch(sql)
                .map_err(|_| OpenError::unavailable("could not initialize tree store schema"))?;
        }
        let changed = transaction
            .execute(
                "INSERT INTO subagents_schema(singleton,instance_id,fingerprint) VALUES(1,?1,?2)",
                rusqlite::params![new_instance_id(), expected_schema_fingerprint()],
            )
            .map_err(|_| OpenError::unavailable("could not initialize tree store identity"))?;
        if changed != 1 {
            return Err(OpenError::unavailable(
                "could not initialize exact tree store identity",
            ));
        }
        transaction
            .pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(|_| OpenError::unavailable("could not version tree store schema"))?;
        transaction
            .commit()
            .map_err(|_| OpenError::unavailable("could not commit tree store schema"))
    }

    fn validate_shape(&self) -> Result<(), String> {
        let mut statement = self.connection.prepare(
            "SELECT name,sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' AND sql IS NOT NULL ORDER BY name",
        ).map_err(|_| "could not inspect tree store shape".to_owned())?;
        let actual = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|_| "could not inspect tree store shape".to_owned())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "tree store shape is invalid".to_owned())?;
        let mut expected = SCHEMA_OBJECTS
            .iter()
            .map(|(name, sql)| ((*name).to_owned(), (*sql).to_owned()))
            .collect::<Vec<_>>();
        expected.sort_by(|left, right| left.0.cmp(&right.0));
        if actual != expected {
            return Err("tree store schema shape does not match its exact revision".to_owned());
        }
        let fingerprint: String = self
            .connection
            .query_row(
                "SELECT fingerprint FROM subagents_schema WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .map_err(|_| "tree store schema fingerprint is missing".to_owned())?;
        if fingerprint != expected_schema_fingerprint() {
            return Err(
                "tree store schema fingerprint does not match its exact revision".to_owned(),
            );
        }
        let integrity: String = self
            .connection
            .query_row("PRAGMA integrity_check(1)", [], |row| row.get(0))
            .map_err(|_| "could not inspect tree store integrity".to_owned())?;
        if integrity != "ok" {
            return Err("tree store integrity check failed".to_owned());
        }
        Ok(())
    }
}

/// The database path with its directory canonicalised, so a symlinked parent
/// cannot redirect the `NOFOLLOW` open.
fn resolved_path(database: &Path) -> Result<PathBuf, OpenError> {
    let parent = database
        .parent()
        .ok_or_else(|| OpenError::unavailable("tree store path has no parent"))?;
    if !parent.exists() {
        std::fs::create_dir_all(parent)
            .map_err(|_| OpenError::unavailable("could not create tree store directory"))?;
    }
    let parent = std::fs::canonicalize(parent)
        .map_err(|_| OpenError::unavailable("could not resolve tree store directory"))?;
    Ok(parent.join(
        database
            .file_name()
            .ok_or_else(|| OpenError::unavailable("tree store file name is missing"))?,
    ))
}

fn new_instance_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests {
    use super::{SCHEMA_OBJECTS, SCHEMA_VERSION, SubagentsDatabase, expected_schema_fingerprint};

    /// Identity golden for the unreleased t7 cohort's durable format.
    #[test]
    fn the_tree_schema_fingerprint_is_pinned() {
        assert_eq!(SCHEMA_VERSION, 8);
        assert_eq!(
            expected_schema_fingerprint(),
            "1c356c82ae91952bd72d6ae9a15f970ee57cfbf23d6fd96ede103f93f41d67a2"
        );
    }

    /// The thirteen objects, in the order the fingerprint is taken over.
    #[test]
    fn the_tree_schema_holds_its_own_identity_row_and_the_agent_tables() {
        let names: Vec<&str> = SCHEMA_OBJECTS.iter().map(|(name, _)| *name).collect();
        assert_eq!(names[0], "subagents_schema", "identity row comes first");
        assert!(
            names[1..].iter().all(|name| name.starts_with("agent_")),
            "{names:?}"
        );
        assert_eq!(names.len(), 13);
    }

    /// A fresh file is created, versioned, fingerprinted, and reopens clean.
    #[test]
    fn a_fresh_tree_database_is_versioned_and_reopens() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("subagents.sqlite3");
        let store = SubagentsDatabase::open(&database).unwrap();
        let version: i64 = store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        drop(store);
        SubagentsDatabase::open(&database).expect("a current file reopens");
    }

    /// A file at any other version is refused as unsupported, never migrated.
    #[test]
    fn a_foreign_schema_version_is_refused_as_unsupported() {
        // Earlier cohorts have no transient-resume command or decision.
        for foreign in [4_i64, 5, 6, 7, 99] {
            let root = tempfile::tempdir().unwrap();
            let database = root.path().join("subagents.sqlite3");
            drop(SubagentsDatabase::open(&database).unwrap());
            let connection = rusqlite::Connection::open(&database).unwrap();
            connection
                .pragma_update(None, "user_version", foreign)
                .unwrap();
            drop(connection);
            let Err(error) = SubagentsDatabase::open(&database) else {
                panic!("a foreign cohort must be refused");
            };
            assert_eq!(error.kind(), super::OpenErrorKind::UnsupportedPersistence);
            assert!(
                error
                    .message()
                    .contains(&format!("subagents schema {foreign}, expected 8")),
                "{error}"
            );
        }
    }
}
