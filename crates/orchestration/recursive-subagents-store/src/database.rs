//! The tree's own database: `subagents.sqlite3` (spec 03 §14).

use std::path::{Path, PathBuf};

use plakous_recursive_subagents::tree::PersistedTree;
use rusqlite::{Connection, OpenFlags};

use crate::admission::{AdmissionError, BUSY_TIMEOUT, Schema};
use crate::rows::load_agent_tree;

/// The tree store's component of the host's state directory key.
pub const GENERATION: i64 = 11;

/// The tree store's current schema revision.
pub const SCHEMA_VERSION: i64 = SCHEMA.version();

/// Frozen base objects for this generation, by name and exact DDL.
pub const SCHEMA_OBJECTS: &[(&str, &str)] = &[
    (
        "subagents_schema",
        "CREATE TABLE subagents_schema(singleton INTEGER PRIMARY KEY CHECK(singleton=1),instance_id TEXT NOT NULL UNIQUE,fingerprint TEXT NOT NULL)",
    ),
    (
        "agent_trees",
        "CREATE TABLE agent_trees(tree_id TEXT PRIMARY KEY,root_agent_id TEXT NOT NULL,revision INTEGER NOT NULL CHECK(revision>0),host_generation INTEGER NOT NULL CHECK(host_generation>0),state TEXT NOT NULL CHECK(state IN('live','interrupted','restored_paused','tree_terminal','closed')),writer_mode TEXT NOT NULL CHECK(writer_mode IN('single_lease')),selected_tuple TEXT NOT NULL,launch_checkout TEXT NOT NULL,snapshot_json TEXT NOT NULL,current_epoch INTEGER NOT NULL CHECK(current_epoch>=0),forked_from TEXT CHECK(forked_from IS NULL OR json_valid(forked_from)))",
    ),
    (
        "agent_nodes",
        "CREATE TABLE agent_nodes(tree_id TEXT NOT NULL,agent_id TEXT NOT NULL,parent_agent_id TEXT,depth INTEGER NOT NULL CHECK(depth>=0 AND depth<=4),description TEXT NOT NULL,prompt TEXT NOT NULL,output_schema_json TEXT NOT NULL,schema_digest TEXT NOT NULL,template TEXT,tuple_id TEXT NOT NULL,account TEXT NOT NULL,effort TEXT,status TEXT NOT NULL CHECK(status IN('pending','running','paused','sealing','completed','interrupted','failed','closed','recovery_required')),workspace_id TEXT NOT NULL,tier TEXT NOT NULL CHECK(tier IN('read_only','execute','mutate')),attempt_id TEXT,session_locator TEXT,path TEXT NOT NULL,branch TEXT,active_turn INTEGER NOT NULL CHECK(active_turn IN(0,1)),fold_order INTEGER CHECK(fold_order>=0),line_id TEXT CHECK(line_id IS NULL OR length(line_id)=36),epoch_no INTEGER NOT NULL CHECK(epoch_no>=0),runner_id TEXT CHECK(runner_id IS NULL OR length(runner_id)>0),workspace_root TEXT,workspace_cwd TEXT,parent_composition TEXT,CHECK((workspace_root IS NULL)=(workspace_cwd IS NULL)),PRIMARY KEY(tree_id,agent_id),FOREIGN KEY(tree_id) REFERENCES agent_trees(tree_id))",
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
        "CREATE TABLE agent_attempts(tree_id TEXT NOT NULL,attempt_id TEXT NOT NULL,agent_id TEXT NOT NULL,status TEXT NOT NULL CHECK(status IN('running','submitted','completed','completed_without_submission','failed','cancelled','recovery_required','closed','blocked','adopted')),turn_token INTEGER NOT NULL CHECK(turn_token>=1),turn_id TEXT,submit_harness_nudge INTEGER NOT NULL CHECK(submit_harness_nudge>=0 AND submit_harness_nudge<=2),candidate_id TEXT,retry_of TEXT,request_key TEXT,started_at INTEGER NOT NULL CHECK(started_at>=0),ended_at INTEGER CHECK(ended_at IS NULL OR ended_at>=0),blocked_kind TEXT,blocked_account TEXT,blocked_resets_at INTEGER,failure_kind TEXT,failure_message TEXT,elapsed_ms INTEGER NOT NULL CHECK(elapsed_ms>=0),turns INTEGER NOT NULL CHECK(turns>=0),tool_calls INTEGER NOT NULL CHECK(tool_calls>=0),input_tokens INTEGER NOT NULL CHECK(input_tokens>=0),output_tokens INTEGER NOT NULL CHECK(output_tokens>=0),spawn_identity TEXT CHECK((request_key IS NULL) = (spawn_identity IS NULL)),output_schema_json TEXT NOT NULL,schema_digest TEXT NOT NULL,applied_terminal_read_seqs TEXT NOT NULL CHECK(json_valid(applied_terminal_read_seqs) AND json_type(applied_terminal_read_seqs)='array'),PRIMARY KEY(tree_id,attempt_id),FOREIGN KEY(tree_id,agent_id) REFERENCES agent_nodes(tree_id,agent_id))",
    ),
    (
        "agent_attempts_request_key",
        "CREATE UNIQUE INDEX agent_attempts_request_key ON agent_attempts(tree_id,request_key) WHERE request_key IS NOT NULL",
    ),
    (
        "agent_candidates",
        "CREATE TABLE agent_candidates(tree_id TEXT NOT NULL,candidate_id TEXT NOT NULL,workspace_id TEXT NOT NULL,producing_agent_id TEXT NOT NULL,producing_attempt_id TEXT NOT NULL,base_commit TEXT NOT NULL,base_content TEXT NOT NULL,base_change_id TEXT,commit_id TEXT NOT NULL,content TEXT NOT NULL,change_id TEXT,sealed_at INTEGER NOT NULL CHECK(sealed_at>=0),adopted_by TEXT,adopted_evidence TEXT NOT NULL,merged_at TEXT,target_before TEXT,kind TEXT NOT NULL CHECK(kind IN('work','conflict')),source_candidate_id TEXT,conflict_target TEXT,conflict_paths TEXT NOT NULL,resolves TEXT,adopted_via TEXT,reverted_at TEXT,reverted_by TEXT,PRIMARY KEY(tree_id,candidate_id),FOREIGN KEY(tree_id) REFERENCES agent_trees(tree_id))",
    ),
    (
        "agent_workspaces",
        "CREATE TABLE agent_workspaces(tree_id TEXT NOT NULL,workspace_id TEXT NOT NULL,owner_agent_id TEXT NOT NULL,path TEXT NOT NULL,mode TEXT NOT NULL CHECK(mode IN('inherit','snapshot','fork')),status TEXT NOT NULL CHECK(status IN('active','adopting','discarding','clean_removed','adopted','discarded')),base_commit TEXT,base_content TEXT,base_change_id TEXT,generation INTEGER NOT NULL CHECK(generation>=0),adopter_agent_id TEXT CHECK(adopter_agent_id IS NULL OR status='adopting'),input_candidate_id TEXT,resolves TEXT,conflict_base TEXT,conflict_target TEXT,conflict_paths TEXT NOT NULL,PRIMARY KEY(tree_id,workspace_id),FOREIGN KEY(tree_id) REFERENCES agent_trees(tree_id))",
    ),
    (
        "agent_commands",
        "CREATE TABLE agent_commands(tree_id TEXT NOT NULL,command_id TEXT NOT NULL,recipient_agent_id TEXT NOT NULL,attempt_id TEXT,kind TEXT NOT NULL CHECK(kind IN('root_batch','start','wake','mail_start','followup','corrective','resume')),canonical_text TEXT NOT NULL,message_ids TEXT NOT NULL,withdrawn INTEGER NOT NULL CHECK(withdrawn IN(0,1)),fact TEXT CHECK(fact IS NULL OR fact IN('accepted','rejected','returned')),turn_id TEXT CHECK(turn_id IS NULL OR fact IS NOT NULL),accepted_seq INTEGER CHECK(accepted_seq IS NULL OR accepted_seq>=1),PRIMARY KEY(tree_id,command_id),FOREIGN KEY(tree_id) REFERENCES agent_trees(tree_id))",
    ),
    (
        "agent_move_seeds",
        "CREATE TABLE agent_move_seeds(tree_id TEXT PRIMARY KEY REFERENCES agent_trees(tree_id) ON DELETE CASCADE,operation TEXT NOT NULL,capsule_digest TEXT NOT NULL CHECK(length(capsule_digest)=64),seed_digest TEXT NOT NULL CHECK(length(seed_digest)=64),seed TEXT NOT NULL CHECK(json_valid(seed)))",
    ),
    (
        "agent_feed_cursors",
        "CREATE TABLE agent_feed_cursors(tree_id TEXT NOT NULL,runtime_session_id TEXT NOT NULL,applied_seq INTEGER NOT NULL CHECK(applied_seq>=0),PRIMARY KEY(tree_id,runtime_session_id),FOREIGN KEY(tree_id) REFERENCES agent_trees(tree_id))",
    ),
    (
        "agent_nodes_tree",
        "CREATE INDEX agent_nodes_tree ON agent_nodes(tree_id,parent_agent_id,agent_id)",
    ),
    (
        "agent_nodes_line",
        "CREATE UNIQUE INDEX agent_nodes_line ON agent_nodes(line_id) WHERE line_id IS NOT NULL",
    ),
    (
        "agent_tree_epochs",
        "CREATE TABLE agent_tree_epochs(tree_id TEXT NOT NULL,epoch_no INTEGER NOT NULL CHECK(epoch_no>=1),snapshot_json TEXT NOT NULL,digest TEXT NOT NULL,PRIMARY KEY(tree_id,epoch_no),FOREIGN KEY(tree_id) REFERENCES agent_trees(tree_id))",
    ),
    (
        "agent_tree_runner_ancestors",
        "CREATE TABLE agent_tree_runner_ancestors(tree_id TEXT NOT NULL,runner_id TEXT NOT NULL CHECK(length(runner_id)>0),commits TEXT NOT NULL CHECK(json_valid(commits) AND json_type(commits)='array'),PRIMARY KEY(tree_id,runner_id),FOREIGN KEY(tree_id) REFERENCES agent_trees(tree_id))",
    ),
    (
        "agent_tree_custody",
        "CREATE TABLE agent_tree_custody(tree_id TEXT PRIMARY KEY,fence INTEGER NOT NULL CHECK(fence>=1),host_id TEXT NOT NULL CHECK(length(host_id)>0),host_instance TEXT NOT NULL CHECK(length(host_instance)>0),host_binary TEXT CHECK(host_binary IS NULL OR length(host_binary)>0),host_name TEXT NOT NULL CHECK(length(host_name)>0),claimed_at INTEGER NOT NULL CHECK(claimed_at>=0),renewed_at INTEGER NOT NULL CHECK(renewed_at>=0),released INTEGER NOT NULL CHECK(released IN(0,1)))",
    ),
];

/// The DDL fingerprint this cohort pins.
#[must_use]
pub fn expected_schema_fingerprint() -> String {
    schema().fingerprint()
}

/// The tree file's admission descriptors.
const SCHEMA: Schema = Schema {
    noun: "tree",
    stem: "subagents",
    archive: "subagents.sqlite3",
    generation: GENERATION,
    objects: SCHEMA_OBJECTS,
    base_fingerprint: "f853d767e6fb5d05a546fd1c32dbb7b160d85b35ae5caeb7fde2efade76ca23a",
    revisions: &[],
};

/// Admission descriptors used by the host's stopped-store and scratch paths.
#[must_use]
pub fn schema() -> Schema {
    crate::test_support::schema(SCHEMA)
}

/// Why the tree database could not be admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenErrorKind {
    /// The file belongs to a schema cohort this build does not read.
    UnsupportedPersistence,
    /// An in-generation revision requires a stopped-store upgrade.
    UpgradePending,
    /// The database was locked by another connection.
    Busy,
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

    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: OpenErrorKind::Unavailable,
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

impl From<AdmissionError> for OpenError {
    fn from(error: AdmissionError) -> Self {
        let (kind, message) = match error {
            AdmissionError::Unsupported(message) => {
                (OpenErrorKind::UnsupportedPersistence, message)
            }
            AdmissionError::Unavailable(message) => (OpenErrorKind::Unavailable, message),
            AdmissionError::UpgradePending(message) => (OpenErrorKind::UpgradePending, message),
            AdmissionError::Busy(message) => (OpenErrorKind::Busy, message),
        };
        Self { kind, message }
    }
}

/// One connection to the tree's database, held for the life of the runtime
/// that owns it.
pub struct SubagentsDatabase {
    pub(crate) connection: Connection,
}

impl SubagentsDatabase {
    pub(crate) fn inspect_tree_with_custody(
        database: &Path,
        tree_id: &str,
    ) -> Result<
        Option<(
            PersistedTree,
            plakous_recursive_subagents::tree::CustodyRecord,
        )>,
        OpenError,
    > {
        Self::inspect_rows(database, None, |tx| {
            let Some(tree) = load_agent_tree(tx, tree_id).map_err(OpenError::unavailable)? else {
                return Ok(None);
            };
            let custody = crate::custody::read(tx, tree_id)
                .map_err(|error| OpenError::unavailable(error.detail))?
                .ok_or_else(|| OpenError::unavailable("move source tree has no custody floor"))?;
            Ok(Some((tree, custody)))
        })
    }

    /// Reads an existing tree on one read-only snapshot without opening a
    /// writer, creating directories, or applying the tree reopen law.
    pub(crate) fn inspect_tree(
        database: &Path,
        tree_id: &str,
    ) -> Result<TreeInspection, OpenError> {
        Self::inspect_rows(database, TreeInspection::StoreAbsent, |tx| {
            load_agent_tree(tx, tree_id)
                .map(|tree| {
                    tree.map_or(TreeInspection::TreeAbsent, |tree| {
                        TreeInspection::Tree(Box::new(tree))
                    })
                })
                .map_err(OpenError::unavailable)
        })
    }

    pub(crate) fn inspect_node_locators(
        database: &Path,
    ) -> Result<Vec<(String, String)>, OpenError> {
        Self::inspect_rows(database, Vec::new(), |tx| {
            let mut rows = tx.prepare("SELECT tree_id,session_locator FROM agent_nodes WHERE session_locator IS NOT NULL ORDER BY tree_id,agent_id").map_err(|_| OpenError::unavailable("could not inspect node bindings"))?;
            rows.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|_| OpenError::unavailable("could not inspect node bindings"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| OpenError::unavailable("could not inspect node bindings"))
        })
    }

    pub(crate) fn tree_ids_with_children_on(
        database: &Path,
        runner_id: &str,
    ) -> Result<Vec<String>, OpenError> {
        Self::inspect_rows(database, Vec::new(), |tx| {
            let unreadable =
                |_| OpenError::unavailable("could not inspect child runner placements");
            let mut statement = tx
                .prepare(
                    "SELECT DISTINCT tree_id FROM agent_nodes
                     WHERE parent_agent_id IS NOT NULL AND runner_id=?1 ORDER BY tree_id",
                )
                .map_err(unreadable)?;
            statement
                .query_map([runner_id], |row| row.get(0))
                .map_err(unreadable)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(unreadable)
        })
    }

    fn inspect_rows<T>(
        database: &Path,
        absent: T,
        read: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T, OpenError>,
    ) -> Result<T, OpenError> {
        let parent = database
            .parent()
            .ok_or_else(|| OpenError::unavailable("tree store path has no parent"))?;
        let parent = match std::fs::canonicalize(parent) {
            Ok(parent) => parent,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(absent);
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
                return Ok(absent);
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
        let tx = connection
            .unchecked_transaction()
            .map_err(|_| OpenError::unavailable("could not start tree inspection"))?;
        schema().require_current(&tx)?;
        read(&tx)
    }

    /// Opens `database`, creating and versioning it when it is new and
    /// refusing it when it belongs to any other cohort.
    pub fn open(database: &Path) -> Result<Self, OpenError> {
        let database = resolved_path(database)?;
        let connection = schema().open_writer(&database, |connection| {
            let _ = connection;
            #[cfg(feature = "test-support")]
            plakous_store::commit_trace::install(connection, "tree").map_err(|_| {
                AdmissionError::Unavailable("could not install commit trace".into())
            })?;
            Ok(())
        })?;
        Ok(Self { connection })
    }

    pub(crate) fn readmit(&self) -> Result<(), OpenError> {
        let snapshot = self
            .connection
            .unchecked_transaction()
            .map_err(|_| OpenError::unavailable("tree admission snapshot could not start"))?;
        schema().require_current(&snapshot).map_err(Into::into)
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
