//! Private Plak product intents and feed-derived views.
//!
//! This store never opens the Plakous runtime database. Stable product and
//! command identities bind each submission to its exact immutable content.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use plakous::sessions::SessionLocator;
use plakous_hosted::error::StoreRefusal;
use plakous_hosted::session_store::RecordStoreError;
use plakous_hosted::sessions::TranscriptRow;
use rusqlite::{Connection, OpenFlags, OptionalExtension as _, Transaction, params};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;

use crate::home::paths::PlakPaths;
use crate::product_config::ProductCompositionSelection;
#[cfg(test)]
use crate::product_config::ProductTargetSelection;
use sessions::{
    ProductCompactReason, QueueDrainState, QueueItem, SessionRecord, SessionWorkflowKind,
    StoreBaseline, digest_parts,
};

pub(crate) const SCHEMA_VERSION: i64 = 32;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_LIST_SESSIONS_SQL: i64 = 500;
const MAX_LIST_PROMPTS_SQL: i64 = 500;
const MAX_LIST_PROMPT_BYTES: usize = 1024 * 1024;
/// Characters of a session's first user prompt a workspace listing projects.
/// The picker's summary column is one line, so the projection is bounded here
/// rather than at the presentation boundary.
const MAX_LIST_FIRST_PROMPT_CHARS: usize = 200;

/// What one workspace listing projects per session, before it is folded back
/// into the record: retained recent prompts, their byte cost, the non-user
/// fallback line, and the session's first user prompt.
type SessionPreview = (Vec<String>, usize, Option<String>, Option<String>);

fn product_sqlite_path(paths: &PlakPaths) -> Result<PathBuf, ProductStoreOpenError> {
    let database = paths.product_database();
    let parent = database
        .parent()
        .ok_or_else(|| ProductStoreOpenError::unavailable("product store path has no parent"))?;
    let parent = std::fs::canonicalize(parent).map_err(|_| {
        ProductStoreOpenError::unavailable("could not resolve product store directory")
    })?;
    Ok(parent.join(
        database.file_name().ok_or_else(|| {
            ProductStoreOpenError::unavailable("product store file name is missing")
        })?,
    ))
}

/// A write transaction the product store could not begin. Another connection
/// holding the write lock past the busy timeout is a transient refusal:
/// nothing was written and the same commit may succeed later.
fn write_refusal(error: rusqlite::Error) -> RecordStoreError {
    match error.sqlite_error_code() {
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
            RecordStoreError::refused(StoreRefusal::Busy, "product store is busy")
        }
        _ => "could not start a product store write".into(),
    }
}

/// Stable admission category for opening the private product database.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductStoreOpenErrorKind {
    /// Persistent state belongs to an unsupported pre-release schema cohort.
    UnsupportedPersistence,
    /// The database could not be opened exactly for any other reason.
    Unavailable,
}

/// Redaction-safe product database admission failure.
#[derive(Debug)]
pub struct ProductStoreOpenError {
    kind: ProductStoreOpenErrorKind,
    message: String,
}

impl ProductStoreOpenError {
    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: ProductStoreOpenErrorKind::Unavailable,
            message: message.into(),
        }
    }

    fn unsupported(message: impl Into<String>) -> Self {
        Self {
            kind: ProductStoreOpenErrorKind::UnsupportedPersistence,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> ProductStoreOpenErrorKind {
        self.kind
    }
}

impl std::fmt::Display for ProductStoreOpenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(formatter)
    }
}

impl std::error::Error for ProductStoreOpenError {}

#[derive(Debug)]
pub struct ProductStore {
    connection: Connection,
    #[cfg(unix)]
    file_identity: (u64, u64),
    /// The collections this handle last loaded or committed per record; a
    /// commit rewrites only what moved. A record this handle never saw is
    /// synced in full.
    baselines: HashMap<String, StoreBaseline>,
    /// Whether the connection commits under `synchronous=FULL`. Projection-only
    /// commits relax it to `NORMAL`; every other commit restores it first.
    synchronous_full: bool,
}

#[cfg(feature = "test-hooks")]
struct SettleCommitTrace;

#[cfg(feature = "test-hooks")]
impl Drop for SettleCommitTrace {
    fn drop(&mut self) {
        plakous_offline_crash_harness::commit_trace::settle();
    }
}

impl ProductStore {
    /// Inspects one existing session record under a read-only SQLite snapshot.
    /// This does not create the home, open a writable store, or change product
    /// schema or session authority.
    pub fn inspect_session_read_only(paths: &PlakPaths, id: &str) -> Result<SessionRecord, String> {
        sessions::canonical_product_session_id(id)?;
        Self::read_only_snapshot(paths, |snapshot| load_record(snapshot, id))
    }

    /// Lists the ids of one workspace's sessions under the same read-only
    /// snapshot discipline as [`Self::inspect_session_read_only`], so a short
    /// id resolves without opening a writable store.
    pub fn session_ids_for_workspace_read_only(
        paths: &PlakPaths,
        workspace: &Path,
    ) -> Result<Vec<String>, String> {
        match std::fs::symlink_metadata(paths.product_database()) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("could not inspect product store: {error}")),
            Ok(_) => {}
        }
        let identity = sessions::workspace_identity(workspace)
            .to_string_lossy()
            .into_owned();
        Self::read_only_snapshot(paths, |snapshot| {
            let mut statement = snapshot
                .prepare("SELECT id FROM sessions WHERE workspace_identity = ?1 ORDER BY id")
                .map_err(|_| "could not query product sessions".to_owned())?;
            statement
                .query_map(params![identity], |row| row.get::<_, String>(0))
                .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
                .map_err(|_| "could not read product sessions".to_owned())
        })
    }

    fn read_only_snapshot<T>(
        paths: &PlakPaths,
        read: impl FnOnce(&Transaction<'_>) -> Result<T, String>,
    ) -> Result<T, String> {
        let database = product_sqlite_path(paths).map_err(|error| error.to_string())?;
        let connection = Connection::open_with_flags(
            database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|_| "could not open product store for inspection".to_owned())?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(|_| "could not configure product store read timeout".to_owned())?;
        connection
            .execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(|_| "could not configure product store inspection".to_owned())?;
        let snapshot = connection
            .unchecked_transaction()
            .map_err(|_| "could not start product store inspection".to_owned())?;
        let version: i64 = snapshot
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|_| "could not inspect product store schema".to_owned())?;
        if version != SCHEMA_VERSION {
            return Err("product store schema is unavailable or unsupported".to_owned());
        }
        validate_schema_shape(&snapshot)?;
        read(&snapshot)
    }

    pub fn open(paths: &PlakPaths) -> Result<Self, ProductStoreOpenError> {
        let _span =
            tracing::info_span!("product_store.open", schema_version = SCHEMA_VERSION).entered();
        Self::open_internal(paths)
    }

    fn open_internal(paths: &PlakPaths) -> Result<Self, ProductStoreOpenError> {
        paths
            .ensure_layout()
            .map_err(ProductStoreOpenError::unavailable)?;
        let database = product_sqlite_path(paths)?;
        let connection = Connection::open_with_flags(
            &database,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|error| {
            ProductStoreOpenError::unavailable(format!("could not open product store: {error}"))
        })?;
        connection.busy_timeout(BUSY_TIMEOUT).map_err(|_| {
            ProductStoreOpenError::unavailable("could not configure product store timeout")
        })?;
        let mode: String = connection
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .map_err(|_| {
                ProductStoreOpenError::unavailable("could not enable product store WAL")
            })?;
        if mode != "wal" {
            return Err(ProductStoreOpenError::unavailable(
                "product store refused WAL mode",
            ));
        }
        connection
            .execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = FULL;")
            .map_err(|_| {
                ProductStoreOpenError::unavailable("could not configure product store durability")
            })?;
        let foreign_keys: i64 = connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .map_err(|_| ProductStoreOpenError::unavailable("could not inspect product store"))?;
        let synchronous: i64 = connection
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .map_err(|_| ProductStoreOpenError::unavailable("could not inspect product store"))?;
        if foreign_keys != 1 || synchronous != 2 {
            return Err(ProductStoreOpenError::unavailable(
                "product store durability configuration was not retained",
            ));
        }
        #[cfg(feature = "test-hooks")]
        plakous_offline_crash_harness::commit_trace::install(&connection, "product")
            .map_err(|_| ProductStoreOpenError::unavailable("could not install commit trace"))?;
        let mut store = Self {
            connection,
            #[cfg(unix)]
            file_identity: (0, 0),
            baselines: HashMap::new(),
            synchronous_full: true,
        };
        store.admit_schema()?;
        #[cfg(unix)]
        {
            let metadata = std::fs::metadata(&database).map_err(|_| {
                ProductStoreOpenError::unavailable("could not inspect product store file identity")
            })?;
            store.file_identity = (metadata.dev(), metadata.ino());
        }
        Ok(store)
    }

    #[cfg(unix)]
    pub fn matches_path(&self, path: &Path) -> bool {
        std::fs::metadata(path)
            .map(|metadata| (metadata.dev(), metadata.ino()) == self.file_identity)
            .unwrap_or(false)
    }

    fn admit_schema(&mut self) -> Result<(), ProductStoreOpenError> {
        #[cfg(feature = "test-hooks")]
        let _settle = SettleCommitTrace;
        let version: i64 = self
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|_| {
                ProductStoreOpenError::unavailable("could not inspect product store schema")
            })?;
        let has_schema: bool = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%')",
                [],
                |row| row.get(0),
            )
            .map_err(|_| {
                ProductStoreOpenError::unavailable("could not inspect product store schema")
            })?;
        if version == 0 && has_schema {
            return Err(ProductStoreOpenError::unavailable(
                "unversioned product store contains unrecognized state; archive product.sqlite3 and runtime.sqlite3 together, then retry",
            ));
        }
        if version == 0 {
            let transaction = self.connection.transaction().map_err(|_| {
                ProductStoreOpenError::unavailable("could not initialize product store")
            })?;
            for (_, sql) in SCHEMA_OBJECTS {
                transaction.execute_batch(sql).map_err(|_| {
                    ProductStoreOpenError::unavailable("could not initialize product store schema")
                })?;
            }
            let changed = transaction
                .execute(
                    "INSERT INTO product_schema(singleton,instance_id,fingerprint) VALUES(1,?1,?2)",
                    params![
                        uuid::Uuid::new_v4().to_string(),
                        expected_schema_fingerprint()
                    ],
                )
                .map_err(|_| {
                    ProductStoreOpenError::unavailable(
                        "could not initialize product store identity",
                    )
                })?;
            if changed != 1 {
                return Err(ProductStoreOpenError::unavailable(
                    "could not initialize exact product store identity",
                ));
            }
            transaction
                .pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(|_| {
                    ProductStoreOpenError::unavailable("could not version product store schema")
                })?;
            transaction.commit().map_err(|_| {
                ProductStoreOpenError::unavailable("could not commit product store schema")
            })?;
            return Ok(());
        }
        if version != SCHEMA_VERSION {
            return Err(ProductStoreOpenError::unsupported(format!(
                "product schema {version}, expected {SCHEMA_VERSION}: this product.sqlite3 was created by an incompatible plak build; move it aside to start fresh"
            )));
        }
        validate_schema_shape(&self.connection).map_err(|error| {
            ProductStoreOpenError::unavailable(format!(
                "{error}; current-schema product state is not authoritative"
            ))
        })?;
        Ok(())
    }

    /// Remember the collections just loaded or committed for `session`.
    fn remember(&mut self, session: &SessionRecord) {
        self.baselines
            .insert(session.id.clone(), StoreBaseline::of(session));
    }

    /// Set the fsync discipline for the next commit on this connection.
    ///
    /// In WAL mode a `NORMAL` commit is ordered but not synced; the next `FULL`
    /// commit syncs the log up to itself, so a projection commit is never
    /// durable later than an authority commit that follows it. Losing only the
    /// newest projection rows on power loss is the accepted cost (spec 02 §2.2).
    fn durability(&mut self, full: bool) -> Result<(), String> {
        if self.synchronous_full == full {
            return Ok(());
        }
        let level = if full { "FULL" } else { "NORMAL" };
        self.connection
            .execute_batch(&format!("PRAGMA synchronous = {level};"))
            .map_err(|_| "could not configure product store durability".to_owned())?;
        self.synchronous_full = full;
        Ok(())
    }

    pub fn commit(&mut self, session: &mut SessionRecord) -> Result<(), String> {
        self.commit_with_submission(session, None, None, None)
            .map_err(String::from)
    }

    pub fn commit_submission(
        &mut self,
        session: &mut SessionRecord,
        submission: &plakous_hosted::sessions::projector::Submission,
    ) -> Result<(), RecordStoreError> {
        self.commit_with_submission(session, Some(submission), None, None)
    }

    pub fn discard_submission(
        &mut self,
        session: &mut SessionRecord,
        command_id: &str,
        pause_cause: Option<plakous_hosted::sessions::projector::HostPauseCause>,
    ) -> Result<(), String> {
        self.commit_with_submission(session, None, Some(command_id), pause_cause)
            .map_err(String::from)
    }

    fn commit_with_submission(
        &mut self,
        session: &mut SessionRecord,
        submission: Option<&plakous_hosted::sessions::projector::Submission>,
        discarded: Option<&str>,
        pause_cause: Option<plakous_hosted::sessions::projector::HostPauseCause>,
    ) -> Result<(), RecordStoreError> {
        #[cfg(feature = "test-hooks")]
        let _settle = SettleCommitTrace;
        let expected = session.store_revision;
        self.durability(true)?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(write_refusal)?;
        let next = commit_in_transaction(
            &transaction,
            session,
            expected,
            self.baselines.get(&session.id),
        )?;
        if let Some(submission) = submission {
            views::insert_submission(&transaction, &session.id, submission)?;
        }
        if let Some(command_id) = discarded {
            transaction
                .execute(
                    "DELETE FROM submissions WHERE session_id=?1 AND command_id=?2",
                    params![session.id, command_id],
                )
                .map_err(|_| "could not discard command submission".to_owned())?;
        }
        if let Some(cause) = pause_cause {
            views::insert_pause_cause(&transaction, &session.id, &cause)?;
        }
        let queue_commands = views::queue_commands_of(&transaction, &session.id)?;
        transaction
            .commit()
            .map_err(|_| "could not durably commit product session".to_owned())?;
        session.store_revision = next;
        session.transcript.mark_committed();
        self.remember(session);
        session.queue_commands = queue_commands;
        Ok(())
    }

    pub fn load(&mut self, id: &str) -> Result<SessionRecord, String> {
        #[cfg(feature = "test-hooks")]
        let _settle = SettleCommitTrace;
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(|_| "could not start product store read".to_owned())?;
        let session = load_record(&transaction, id)?;
        let mut baseline = StoreBaseline::of(&session);
        // The joined read can omit consumed items still present in the intent table.
        baseline.pending_queue = load_queue(&transaction, id)?;
        transaction
            .commit()
            .map_err(|_| "could not finish product store read".to_owned())?;
        self.baselines.insert(session.id.clone(), baseline);
        Ok(session)
    }
}

fn load_record(connection: &Connection, id: &str) -> Result<SessionRecord, String> {
    let mut session = connection
            .query_row(
                "SELECT revision, create_operation_id, workspace_root, created_at, updated_at, account, model, effort, title, source_session_id, source_operation_id, locator, composition, target_selection, frozen_tools FROM sessions WHERE id = ?1",
                [id],
                |row| {
                    let locator: Option<String> = row.get(11)?;
                    Ok(SessionRecord {
                        store_revision: from_i64(row.get(0)?)?,
                        id: id.to_owned(),
                        create_operation_id: row.get(1)?,
                        workspace_root: PathBuf::from(row.get::<_, String>(2)?),
                        created_at_unix: from_i64(row.get(3)?)?,
                        updated_at_unix: from_i64(row.get(4)?)?,
                        account: row.get(5)?, model: row.get(6)?, effort: row.get(7)?, title: row.get(8)?,
                        composition: ProductCompositionSelection::parse_config(&row.get::<_, String>(12)?).map_err(|_| rusqlite::Error::InvalidQuery)?,
                        target_selection: serde_json::from_str(&row.get::<_, String>(13)?).map_err(|_| rusqlite::Error::InvalidQuery)?,
                        source_product_session_id: row.get(9)?, source_workflow_operation_id: row.get(10)?,
                        plakous_locator: locator.map(|value| value.parse::<SessionLocator>().map_err(|_| rusqlite::Error::InvalidQuery)).transpose()?,
                        frozen_tools: row.get::<_, Option<String>>(14)?.map(|value| plakous_hosted::frozen_tools::FrozenRootTools::decode(&value).map_err(|_| rusqlite::Error::InvalidQuery)).transpose()?,
                        transcript: sessions::ProductTranscript::default(), first_prompt: None, prompt_history: Vec::new(), pending_queue: Vec::new(), queue_commands: sessions::QueueCommands::default(), queue_projection_debt: false,
                        queue_drain_state: QueueDrainState::LiveAutoDrain,
                        root_turn_count: 0,
                        child_wake_suppressed: false,
                        tool_call_count: 0,
                        compaction_count: 0,
                    })
                },
            )
            .map_err(|error| if matches!(error, rusqlite::Error::QueryReturnedNoRows) { "session not found".to_owned() } else { "product session row is invalid".to_owned() })?;
    session.pending_queue = load_queue(connection, id)?;
    let (transcript, views, queue_commands) = views::transcript_of(connection, id)?;
    session.transcript = transcript;
    let intents = views::intents_of(connection, id)?;
    let empty = plakous_hosted::sessions::projector::SessionViews::empty();
    let joined = views.as_ref().unwrap_or(&empty);
    session.child_wake_suppressed = joined.wake_suppressed(&intents);
    if joined.pause_cause(&intents).is_some() {
        session.queue_drain_state = QueueDrainState::RestoredPaused;
    }
    if let Some(views) = &views {
        session.plakous_locator = views
            .state
            .head
            .map(|head| SessionLocator::new(head.session_id, head.branch_id, head.checkpoint_id));
        // Spec 02 §2.6: the counters are views.
        session.root_turn_count = views.state.counters.root_turns;
        session.tool_call_count = views.state.counters.tool_calls;
        session.compaction_count = views.state.counters.compactions;
    }
    session.prompt_history = load_prompt_history(connection, id)?;
    session.validate_for_store()?;
    validate_loaded_bindings(connection, &session)?;
    session.queue_commands = queue_commands;
    session
        .pending_queue
        .retain(|item| !session.queue_commands.is_consumed(item));
    session.pause_restored_work();
    Ok(session)
}

fn validate_loaded_bindings(
    connection: &Connection,
    session: &SessionRecord,
) -> Result<(), String> {
    let create:Option<(String,String,String,Option<String>)>=connection.query_row("SELECT kind,owner_session_id,binding_digest,terminal_disposition FROM identity_bindings WHERE identity=?1",[&session.create_operation_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(|_|"could not inspect create identity".to_owned())?;
    if create
        != Some((
            "create".to_owned(),
            session.id.clone(),
            session.id.clone(),
            None,
        ))
    {
        return Err("session create identity contradicts global registry".to_owned());
    }
    for item in &session.pending_queue {
        let queue:Option<(String,String,String,Option<String>)>=connection.query_row("SELECT kind,owner_session_id,binding_digest,terminal_disposition FROM identity_bindings WHERE identity=?1",[&item.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(|_|"could not inspect queue identity".to_owned())?;
        if queue
            != Some((
                "queue".to_owned(),
                session.id.clone(),
                session.id.clone(),
                None,
            ))
        {
            return Err("active queue identity contradicts global registry".to_owned());
        }
    }
    Ok(())
}

impl ProductStore {
    /// Other sessions that would lose something if `id` went away: those that
    /// continue from it (rebind and clear sources) and those whose
    /// head lives in the same runtime session, as a fork's does. A session
    /// carried forward from it holds its own copy and does not depend on it.
    pub fn session_dependents(
        &mut self,
        id: &str,
        runtime_session: plakous::model::SessionId,
    ) -> Result<Vec<String>, String> {
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(|_| "could not start product dependency read".to_owned())?;
        let mut statement = transaction
            .prepare(
                "SELECT s.id, s.source_session_id, s.source_operation_id, CASE WHEN v.session_id IS NULL THEN s.locator ELSE v.head_locator END FROM sessions s LEFT JOIN session_views v ON v.session_id=s.id WHERE s.id <> ?1 ORDER BY s.id",
            )
            .map_err(|_| "could not query product session dependents".to_owned())?;
        let rows = statement
            .query_map([id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(|_| "could not query product session dependents".to_owned())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "could not read product session dependents".to_owned())?;
        let mut dependents = Vec::new();
        for (other, source, operation, locator) in rows {
            let carried = operation.as_deref().is_some_and(|operation| {
                operation.starts_with(plakous_hosted::sessions::CARRY_FORWARD_OPERATION_PREFIX)
            });
            let continues = source.as_deref() == Some(id) && !carried;
            let shares_runtime = locator
                .map(|locator| locator.parse::<SessionLocator>())
                .transpose()
                .map_err(|_| format!("product session {other} has an invalid runtime locator"))?
                .is_some_and(|locator| locator.session_id() == runtime_session);
            if continues || shares_runtime {
                dependents.push(other);
            }
        }
        Ok(dependents)
    }

    /// Every product session in this home, across workspaces, with its
    /// recorded runtime locator.
    pub fn session_locators(&mut self) -> Result<Vec<(String, Option<SessionLocator>)>, String> {
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(|_| "could not start product session read".to_owned())?;
        let mut statement = transaction
            .prepare("SELECT s.id, CASE WHEN v.session_id IS NULL THEN s.locator ELSE v.head_locator END FROM sessions s LEFT JOIN session_views v ON v.session_id=s.id ORDER BY s.id")
            .map_err(|_| "could not query product sessions".to_owned())?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .map_err(|_| "could not query product sessions".to_owned())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "could not read product sessions".to_owned())?;
        rows.into_iter()
            .map(|(id, locator)| {
                let locator = locator
                    .map(|locator| locator.parse::<SessionLocator>())
                    .transpose()
                    .map_err(|_| format!("product session {id} has an invalid runtime locator"))?;
                Ok((id, locator))
            })
            .collect()
    }

    /// Deletes one product session record and the rows it owns, at exactly
    /// the revision the caller checked. Identity bindings stay, so a replayed
    /// operation is refused rather than re-run.
    pub fn delete_session(&mut self, id: &str, expected_revision: u64) -> Result<(), String> {
        #[cfg(feature = "test-hooks")]
        let _settle = SettleCommitTrace;
        self.durability(true)?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| String::from(write_refusal(error)))?;
        let deleted = transaction
            .execute(
                "DELETE FROM sessions WHERE id = ?1 AND revision = ?2",
                params![id, to_i64(expected_revision)?],
            )
            .map_err(|_| "could not delete the product session".to_owned())?;
        if deleted != 1 {
            return Err(format!(
                "product session {id} changed before it could be deleted"
            ));
        }
        transaction
            .commit()
            .map_err(|_| "could not durably delete the product session".to_owned())?;
        self.baselines.remove(id);
        Ok(())
    }

    pub fn list_for_workspace(&mut self, workspace: &Path) -> Result<Vec<SessionRecord>, String> {
        self.list_for_workspace_with_query_count(workspace)
            .map(|(sessions, _)| sessions)
    }

    fn list_for_workspace_with_query_count(
        &mut self,
        workspace: &Path,
    ) -> Result<(Vec<SessionRecord>, usize), String> {
        #[cfg(feature = "test-hooks")]
        let _settle = SettleCommitTrace;
        let identity = sessions::workspace_identity(workspace)
            .to_string_lossy()
            .into_owned();
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(|_| "could not start product listing read".to_owned())?;
        let mut query_count = 0usize;
        let mut statement = transaction.prepare(
            "SELECT id,create_operation_id,workspace_root,created_at,updated_at,account,model,effort,title,source_session_id,source_operation_id,CASE WHEN v.session_id IS NULL THEN sessions.locator ELSE v.head_locator END,composition,target_selection,COALESCE(v.root_turn_count,0),COALESCE(v.tool_call_count,0),COALESCE(v.compaction_count,0) FROM sessions LEFT JOIN session_views v ON v.session_id=sessions.id WHERE workspace_identity = ?1 ORDER BY updated_at DESC, id ASC LIMIT ?2"
        ).map_err(|_| "could not query product sessions".to_owned())?;
        query_count += 1;
        let mut sessions = statement
            .query_map(params![identity, MAX_LIST_SESSIONS_SQL + 1], |row| {
                let id: String = row.get(0)?;
                let locator: Option<String> = row.get(11)?;
                Ok(SessionRecord {
                    store_revision: 0,
                    id,
                    create_operation_id: row.get(1)?,
                    workspace_root: PathBuf::from(row.get::<_, String>(2)?),
                    created_at_unix: from_i64(row.get(3)?)?,
                    updated_at_unix: from_i64(row.get(4)?)?,
                    account: row.get(5)?,
                    model: row.get(6)?,
                    effort: row.get(7)?,
                    composition: ProductCompositionSelection::parse_config(
                        &row.get::<_, String>(12)?,
                    )
                    .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    target_selection: serde_json::from_str(&row.get::<_, String>(13)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    title: row.get(8)?,
                    source_product_session_id: row.get(9)?,
                    source_workflow_operation_id: row.get(10)?,
                    plakous_locator: locator
                        .map(|value| value.parse().map_err(|_| rusqlite::Error::InvalidQuery))
                        .transpose()?,
                    frozen_tools: None,
                    transcript: sessions::ProductTranscript::default(),
                    first_prompt: None,
                    prompt_history: Vec::new(),
                    pending_queue: Vec::new(),
                    queue_commands: sessions::QueueCommands::default(),
                    queue_projection_debt: false,
                    queue_drain_state: QueueDrainState::RestoredPaused,
                    root_turn_count: from_i64(row.get(14)?)?,
                    child_wake_suppressed: false,
                    tool_call_count: from_i64(row.get(15)?)?,
                    compaction_count: from_i64(row.get(16)?)?,
                })
            })
            .map_err(|_| "could not query product sessions".to_owned())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "product session listing row is invalid".to_owned())?;
        drop(statement);
        if sessions.len() > usize::try_from(MAX_LIST_SESSIONS_SQL).unwrap_or(usize::MAX) {
            return Err("product session listing exceeds the supported result bound".to_owned());
        }
        for session in &sessions {
            sessions::canonical_product_session_id(&session.id)?;
        }

        let mut statement = transaction
            .prepare(
                "SELECT selected.id,
                        (SELECT json_group_array(recent.text)
                         FROM (
                           SELECT json_extract(row,'$.text') AS text
                           FROM (SELECT session_id,anchor,lane,ordinal,row FROM transcript_rows UNION ALL SELECT session_id,anchor,1,ordinal,row FROM operator_notices)
                           WHERE session_id=selected.id
                             AND json_extract(row,'$.kind')='user'
                             AND length(trim(json_extract(row,'$.text')))>0
                           ORDER BY anchor DESC,lane DESC,ordinal DESC
                           LIMIT ?2
                         ) AS recent),
                        (SELECT json_extract(row,'$.text')
                         FROM (SELECT session_id,anchor,lane,ordinal,row FROM transcript_rows UNION ALL SELECT session_id,anchor,1,ordinal,row FROM operator_notices)
                         WHERE session_id=selected.id
                           AND json_extract(row,'$.kind')='assistant'
                           AND length(trim(json_extract(row,'$.text')))>0
                         ORDER BY anchor,lane,ordinal
                         LIMIT 1),
                        (SELECT json_extract(row,'$.text')
                         FROM (SELECT session_id,anchor,lane,ordinal,row FROM transcript_rows UNION ALL SELECT session_id,anchor,1,ordinal,row FROM operator_notices)
                         WHERE session_id=selected.id
                           AND json_extract(row,'$.kind')='user'
                           AND length(trim(json_extract(row,'$.text')))>0
                         ORDER BY anchor,lane,ordinal
                         LIMIT 1)
                 FROM (
                   SELECT id
                   FROM sessions
                   WHERE workspace_identity=?1
                   ORDER BY updated_at DESC,id ASC
                   LIMIT ?3
                 ) AS selected",
            )
            .map_err(|_| "could not query product session previews".to_owned())?;
        query_count += 1;
        let projections = statement
            .query_map(
                params![identity, MAX_LIST_PROMPTS_SQL, MAX_LIST_SESSIONS_SQL],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .map_err(|_| "could not query product session previews".to_owned())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "product session preview is invalid".to_owned())?;
        drop(statement);
        let mut previews: HashMap<String, SessionPreview> = HashMap::new();
        for (session_id, encoded_prompts, fallback, first_prompt) in projections {
            let prompts = serde_json::from_str::<Vec<String>>(&encoded_prompts)
                .map_err(|_| "product session prompt preview is invalid".to_owned())?;
            let (retained, bytes, retained_fallback, retained_first) =
                previews.entry(session_id).or_default();
            for line in prompts {
                let next = bytes.saturating_add(line.len());
                if next <= MAX_LIST_PROMPT_BYTES {
                    *bytes = next;
                    retained.push(line);
                }
            }
            *retained_fallback = fallback;
            *retained_first = first_prompt;
        }
        for session in &mut sessions {
            if let Some((mut prompts, _, fallback, first_prompt)) = previews.remove(&session.id) {
                // The listing's `transcript` projection is the most recent
                // prompts; the picker's summary is the first one, so it is its
                // own bounded column rather than a guess taken off that tail.
                session.first_prompt = first_prompt.and_then(|line| {
                    let body = line.trim();
                    (!body.is_empty())
                        .then(|| body.chars().take(MAX_LIST_FIRST_PROMPT_CHARS).collect())
                });
                prompts.reverse();
                let rows: Vec<TranscriptRow> = if prompts.is_empty() {
                    fallback.into_iter().map(TranscriptRow::assistant).collect()
                } else {
                    prompts.into_iter().map(TranscriptRow::user).collect()
                };
                session.transcript =
                    sessions::ProductTranscript::from_rows(rows.into_iter().map(|row| (row, None)));
            }
        }
        transaction
            .commit()
            .map_err(|_| "could not finish product listing read".to_owned())?;
        Ok((sessions, query_count))
    }
}

fn insert_session(tx: &Transaction<'_>, s: &SessionRecord) -> Result<(), String> {
    let workspace = s.workspace_root.to_string_lossy().into_owned();
    let workspace_identity = sessions::workspace_identity(&s.workspace_root)
        .to_string_lossy()
        .into_owned();
    let locator = s.plakous_locator.map(|value| value.to_string());
    let changed = tx.execute(
        "INSERT INTO sessions(id, revision, create_operation_id, workspace_root, workspace_identity, created_at, updated_at, account, model, effort, title, source_session_id, source_operation_id, locator, composition, target_selection, frozen_tools) VALUES (?1,1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
        params![s.id, s.create_operation_id, workspace, workspace_identity, to_i64(s.created_at_unix)?, to_i64(s.updated_at_unix)?, s.account, s.model, s.effort, s.title, s.source_product_session_id, s.source_workflow_operation_id, locator, s.composition.as_str(), serde_json::to_string(&s.target_selection).map_err(|_|"target selection is not serializable")?, encoded_frozen_tools(s)?],
    ).map_err(|_| "product session identity already exists or is invalid".to_owned())?;
    if changed != 1 {
        return Err("product session insert was not exact".to_owned());
    }
    Ok(())
}

fn commit_in_transaction(
    tx: &Transaction<'_>,
    session: &SessionRecord,
    expected: u64,
    baseline: Option<&StoreBaseline>,
) -> Result<u64, String> {
    // Validate before any identity claim or revision change.
    session.validate_for_store()?;
    let next = if expected == 0 {
        insert_session(tx, session)?;
        bind_identity(
            tx,
            "create",
            &session.create_operation_id,
            &session.id,
            &session.id,
            None,
            None,
        )?;
        1
    } else {
        let next = expected
            .checked_add(1)
            .ok_or_else(|| "product session revision is exhausted".to_owned())?;
        if update_session(tx, session, expected, next)? != 1 {
            return Err("product session changed concurrently".to_owned());
        }
        next
    };
    // A record this handle never loaded or committed has no baseline: every
    // collection is synced, which is also what a first commit needs.
    let resync_all = expected == 0 || baseline.is_none();
    let moved = |same: bool| resync_all || !same;
    if moved(baseline.is_some_and(|b| b.pending_queue == session.pending_queue)) {
        sync_queue(tx, session)?;
    }
    views::write_notices(tx, session, expected == 0)?;
    if moved(baseline.is_some_and(|b| b.prompt_history == session.prompt_history)) {
        sync_prompt_history(tx, session)?;
    }
    Ok(next)
}

fn update_session(
    tx: &Transaction<'_>,
    s: &SessionRecord,
    expected: u64,
    next: u64,
) -> Result<usize, String> {
    // A loaded record carries the view head, not a replacement genesis binding.
    tx.execute(
        "UPDATE sessions SET revision=?2, workspace_root=?4, workspace_identity=?5, updated_at=?7, account=?8, model=?9, effort=?10, title=?11, source_session_id=?12, source_operation_id=?13, locator=COALESCE(locator,?14), composition=?15, target_selection=?17, frozen_tools=COALESCE(frozen_tools,?18) WHERE id=?1 AND revision=?3 AND create_operation_id=?16 AND created_at=?6",
        params![s.id, to_i64(next)?, to_i64(expected)?, s.workspace_root.to_string_lossy(), sessions::workspace_identity(&s.workspace_root).to_string_lossy(), to_i64(s.created_at_unix)?, to_i64(s.updated_at_unix)?, s.account, s.model, s.effort, s.title, s.source_product_session_id, s.source_workflow_operation_id, s.plakous_locator.map(|v| v.to_string()), s.composition.as_str(), s.create_operation_id, serde_json::to_string(&s.target_selection).map_err(|_|"target selection is not serializable")?, encoded_frozen_tools(s)?],
    ).map_err(|_| "could not update product session".to_owned())
}

/// The root's frozen tools are written once, with the first runtime
/// creation; a later save never replaces them.
fn encoded_frozen_tools(s: &SessionRecord) -> Result<Option<String>, String> {
    s.frozen_tools
        .as_ref()
        .map(plakous_hosted::frozen_tools::FrozenRootTools::encode)
        .transpose()
}

fn sync_queue(tx: &Transaction<'_>, s: &SessionRecord) -> Result<(), String> {
    for item in &s.pending_queue {
        bind_identity(tx, "queue", &item.id, &s.id, &s.id, None, None)?;
    }
    let existing = query_collect(
        tx,
        "SELECT item_id,revision,text FROM queue_items WHERE session_id=?1",
        &s.id,
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        },
    )?;
    for (item_id, revision, text) in existing
        .iter()
        .filter(|(id, _, _)| !s.pending_queue.iter().any(|item| &item.id == id))
    {
        let binding = digest_parts(&[&s.id, &revision.to_string(), &digest_parts(&[text])]);
        let changed=tx.execute("UPDATE identity_bindings SET binding_digest=?3,terminal_disposition=?4,terminal_outcome=?5 WHERE identity=?1 AND kind='queue' AND owner_session_id=?2 AND binding_digest=?2 AND terminal_disposition IS NULL",params![item_id,s.id,binding,"deleted",None::<&str>]).map_err(|_|"could not seal removed queue identity".to_owned())?;
        if changed != 1 {
            return Err("queue tombstone contradicts durable state".to_owned());
        }
    }
    delete_missing(
        tx,
        "queue_items",
        &s.id,
        s.pending_queue.iter().map(|item| item.id.as_str()),
    )?;
    let durable_order = query_collect(
        tx,
        "SELECT item_id FROM queue_items WHERE session_id=?1 ORDER BY position",
        &s.id,
        |row| row.get::<_, String>(0),
    )?;
    if durable_order
        != s.pending_queue
            .iter()
            .map(|item| item.id.clone())
            .collect::<Vec<_>>()
    {
        tx.execute(
            "UPDATE queue_items SET position=position+1000000 WHERE session_id=?1",
            [&s.id],
        )
        .map_err(|_| "could not stage product queue reorder".to_owned())?;
    }
    for (position, item) in s.pending_queue.iter().enumerate() {
        let changed = tx.execute("INSERT INTO queue_items(session_id,item_id,revision,position,text,submitted_at) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(item_id) DO UPDATE SET revision=excluded.revision,position=excluded.position,text=excluded.text WHERE session_id=excluded.session_id AND ((queue_items.revision=excluded.revision AND queue_items.text=excluded.text) OR queue_items.revision+1=excluded.revision)",
            params![s.id,item.id,to_i64(item.revision)?,i64::try_from(position).map_err(|_| "queue position is invalid")?,item.text,to_i64(item.submitted_at_ms)?])
            .map_err(|_| "could not commit product queue".to_owned())?;
        if changed != 1 {
            return Err("queue identity contradicts durable product state".to_owned());
        }
        // Attachments are immutable per item: written once with the item,
        // removed with it by the foreign-key cascade.
        if !existing.iter().any(|(id, _, _)| id == &item.id) {
            for (ordinal, image) in item.images.iter().enumerate() {
                tx.execute(
                    "INSERT INTO queue_attachments(item_id,ordinal,name,media_type,bytes) VALUES(?1,?2,?3,?4,?5)",
                    params![
                        item.id,
                        i64::try_from(ordinal).map_err(|_| "attachment ordinal is invalid")?,
                        image.name,
                        image.media_type,
                        image.bytes
                    ],
                )
                .map_err(|_| "could not commit product queue attachments".to_owned())?;
            }
        }
    }
    Ok(())
}

fn sync_prompt_history(tx: &Transaction<'_>, s: &SessionRecord) -> Result<(), String> {
    let old = query_collect(
        tx,
        "SELECT position,prompt FROM prompt_history WHERE session_id=?1 ORDER BY position",
        &s.id,
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
    )?;
    let overlap = (0..=old.len().min(s.prompt_history.len()))
        .rev()
        .find(|count| {
            old[old.len() - count..]
                .iter()
                .map(|(_, prompt)| prompt)
                .eq(s.prompt_history[..*count].iter())
        })
        .unwrap_or(0);
    for (position, _) in &old[..old.len() - overlap] {
        if tx
            .execute(
                "DELETE FROM prompt_history WHERE session_id=?1 AND position=?2",
                params![s.id, position],
            )
            .map_err(|_| "could not prune product prompt history".to_owned())?
            != 1
        {
            return Err("product prompt history changed concurrently".to_owned());
        }
    }
    let mut position = old.last().map_or(Ok(0), |(position, _)| {
        position
            .checked_add(1)
            .ok_or_else(|| "prompt history position is exhausted".to_owned())
    })?;
    for prompt in &s.prompt_history[overlap..] {
        let changed = tx
            .execute(
                "INSERT INTO prompt_history(session_id,position,prompt) VALUES(?1,?2,?3)",
                params![s.id, position, prompt],
            )
            .map_err(|_| "could not commit product prompt history".to_owned())?;
        if changed != 1 {
            return Err("could not commit exact product prompt history".to_owned());
        }
        position = position
            .checked_add(1)
            .ok_or_else(|| "prompt history position is exhausted".to_owned())?;
    }
    Ok(())
}

fn delete_missing<'a>(
    tx: &Transaction<'_>,
    table: &str,
    session: &str,
    keep: impl Iterator<Item = &'a str>,
) -> Result<(), String> {
    let key = if table == "queue_items" {
        "item_id"
    } else {
        "operation_id"
    };
    let mut statement = tx
        .prepare(&format!("SELECT {key} FROM {table} WHERE session_id=?1"))
        .map_err(|_| "could not inspect product rows".to_owned())?;
    let existing = statement
        .query_map([session], |row| row.get::<_, String>(0))
        .map_err(|_| "could not inspect product rows".to_owned())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "product row identity is invalid".to_owned())?;
    let keep = keep.collect::<HashSet<_>>();
    for id in existing {
        if !keep.contains(id.as_str()) {
            let changed = tx
                .execute(
                    &format!("DELETE FROM {table} WHERE session_id=?1 AND {key}=?2"),
                    params![session, id],
                )
                .map_err(|_| "could not remove product row".to_owned())?;
            if changed != 1 {
                return Err("product row changed during exact removal".to_owned());
            }
        }
    }
    Ok(())
}

fn bind_identity(
    tx: &Transaction<'_>,
    kind: &str,
    identity: &str,
    owner: &str,
    binding: &str,
    terminal_disposition: Option<&str>,
    terminal_outcome: Option<&str>,
) -> Result<(), String> {
    let changed = tx.execute(
        "INSERT INTO identity_bindings(identity,kind,owner_session_id,binding_digest,terminal_disposition,terminal_outcome) VALUES(?1,?2,?3,?4,?5,?6)
         ON CONFLICT(identity) DO UPDATE SET terminal_disposition=excluded.terminal_disposition,terminal_outcome=excluded.terminal_outcome
         WHERE kind=excluded.kind AND owner_session_id=excluded.owner_session_id
           AND binding_digest=excluded.binding_digest
           AND (terminal_disposition IS NULL OR terminal_disposition=excluded.terminal_disposition)
           AND (terminal_outcome IS NULL OR terminal_outcome IS excluded.terminal_outcome)
           AND NOT(terminal_disposition IS NOT NULL AND excluded.terminal_disposition IS NULL)",
        params![identity, kind, owner, binding, terminal_disposition,terminal_outcome],
    ).map_err(|_| "stable product identity contradicts durable state".to_owned())?;
    if changed != 1 {
        return Err("stable product identity contradicts durable state".to_owned());
    }
    Ok(())
}

fn load_queue(c: &Connection, id: &str) -> Result<Vec<QueueItem>, String> {
    query_collect(
        c,
        "SELECT item_id,revision,text,submitted_at FROM queue_items WHERE session_id=?1 ORDER BY position",
        id,
        |row| {
            Ok(QueueItem {
                id: row.get(0)?,
                revision: from_i64(row.get(1)?)?,
                text: row.get(2)?,
                submitted_at_ms: from_i64(row.get(3)?)?,
                images: std::sync::Arc::default(),
            })
        },
    )?
    .into_iter()
    .map(|mut item| {
        item.images = query_collect(
            c,
            "SELECT name,media_type,bytes FROM queue_attachments WHERE item_id=?1 ORDER BY ordinal",
            &item.id,
            |row| {
                Ok(plakous_host_protocol::DraftImage {
                    name: row.get(0)?,
                    media_type: row.get(1)?,
                    bytes: row.get(2)?,
                })
            },
        )?
        .into();
        Ok(item)
    })
    .collect()
}

fn redundant_exposed_name(tool: &plakous_host_protocol::views::transcript::ToolItem) -> bool {
    tool.exposed_name.as_deref() == Some(tool.name.as_str())
}

/// A tool row's exposed name is present only when it differs from its
/// canonical name; one equal to it is dropped.
pub(crate) fn normalize_exposed_name(row: &mut TranscriptRow) {
    if let TranscriptRow::Tool(tool) = row
        && redundant_exposed_name(tool)
    {
        tool.exposed_name = None;
    }
}

fn load_prompt_history(c: &Connection, id: &str) -> Result<Vec<String>, String> {
    query_collect(
        c,
        "SELECT prompt FROM prompt_history WHERE session_id=?1 ORDER BY position",
        id,
        |r| r.get(0),
    )
}
fn query_collect<T>(
    c: &Connection,
    sql: &str,
    id: &str,
    map: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>, String> {
    let mut s = c
        .prepare(sql)
        .map_err(|_| "could not query product rows".to_owned())?;
    s.query_map([id], map)
        .map_err(|_| "could not query product rows".to_owned())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "product row is invalid".to_owned())
}

fn decode_workflow_kind(
    kind: &str,
    reason: Option<String>,
    selection: Option<String>,
) -> rusqlite::Result<SessionWorkflowKind> {
    match kind {
        "compact" => Ok(SessionWorkflowKind::Compact(
            ProductCompactReason::parse(reason.as_deref().unwrap_or(""))
                .ok_or(rusqlite::Error::InvalidQuery)?,
        )),
        "clear" => Ok(SessionWorkflowKind::Clear),
        "fork" => Ok(SessionWorkflowKind::Fork),
        "rebind" => Ok(SessionWorkflowKind::Rebind(Box::new(
            serde_json::from_str(&selection.ok_or(rusqlite::Error::InvalidQuery)?)
                .map_err(|_| rusqlite::Error::InvalidQuery)?,
        ))),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}
fn to_i64(v: u64) -> Result<i64, String> {
    i64::try_from(v).map_err(|_| "product integer exceeds SQLite range".to_owned())
}
fn from_i64(v: i64) -> rusqlite::Result<u64> {
    u64::try_from(v).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, v))
}

const SCHEMA_OBJECTS: &[(&str, &str)] = &[
    (
        "product_schema",
        "CREATE TABLE product_schema(singleton INTEGER PRIMARY KEY CHECK(singleton=1),instance_id TEXT NOT NULL UNIQUE,fingerprint TEXT NOT NULL)",
    ),
    (
        "sessions",
        "CREATE TABLE sessions(id TEXT PRIMARY KEY,revision INTEGER NOT NULL CHECK(revision>0),create_operation_id TEXT NOT NULL UNIQUE,workspace_root TEXT NOT NULL,workspace_identity TEXT NOT NULL,created_at INTEGER NOT NULL CHECK(created_at>=0),updated_at INTEGER NOT NULL CHECK(updated_at>=0),account TEXT,model TEXT,effort TEXT,title TEXT,source_session_id TEXT,source_operation_id TEXT,locator TEXT, composition TEXT NOT NULL DEFAULT 'plakous-default' CHECK(composition IN('direct','plakous-default','native-ptc','js-ptc','py-ptc')), target_selection TEXT NOT NULL DEFAULT '\"native\"', frozen_tools TEXT CHECK(frozen_tools IS NULL OR json_valid(frozen_tools)))",
    ),
    (
        "sessions_workspace_updated",
        "CREATE INDEX sessions_workspace_updated ON sessions(workspace_identity,updated_at DESC,id)",
    ),
    (
        "identity_bindings",
        "CREATE TABLE identity_bindings(identity TEXT PRIMARY KEY,kind TEXT NOT NULL CHECK(kind IN('create','queue','delivery','command','workflow')),owner_session_id TEXT NOT NULL,binding_digest TEXT NOT NULL,terminal_disposition TEXT,terminal_outcome TEXT,CHECK((kind='create' AND terminal_disposition IS NULL AND terminal_outcome IS NULL) OR (kind='queue' AND ((terminal_disposition IS NULL AND terminal_outcome IS NULL) OR (terminal_disposition='consumed' AND terminal_outcome IS NOT NULL) OR (terminal_disposition='deleted' AND terminal_outcome IS NULL))) OR (kind IN('delivery','command') AND terminal_disposition IN('rejected','product_committed') AND terminal_outcome IS NULL) OR (kind='workflow' AND ((terminal_disposition IS NULL AND terminal_outcome IS NULL) OR (terminal_disposition='product_committed' AND terminal_outcome IS NOT NULL) OR (terminal_disposition IN('terminal_no_change','discarded') AND terminal_outcome IS NULL)))))",
    ),
    (
        "queue_items",
        "CREATE TABLE queue_items(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,item_id TEXT PRIMARY KEY,revision INTEGER NOT NULL CHECK(revision>0),position INTEGER NOT NULL CHECK(position>=0),text TEXT NOT NULL,submitted_at INTEGER NOT NULL CHECK(submitted_at>=0),UNIQUE(session_id,position))",
    ),
    (
        "queue_attachments",
        "CREATE TABLE queue_attachments(item_id TEXT NOT NULL REFERENCES queue_items(item_id) ON DELETE CASCADE,ordinal INTEGER NOT NULL CHECK(ordinal>=0),name TEXT NOT NULL,media_type TEXT NOT NULL,bytes BLOB NOT NULL,PRIMARY KEY(item_id,ordinal))",
    ),
    (
        "workflows",
        "CREATE TABLE workflows(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,operation_id TEXT PRIMARY KEY,source_locator TEXT NOT NULL,target_session_id TEXT,target_locator TEXT,kind TEXT NOT NULL CHECK(kind IN('compact','clear','fork','rebind')),compact_reason TEXT,rebind_selection TEXT,state TEXT NOT NULL CHECK(state IN('prepared','runtime_published','recovery_required','product_committed','terminal_no_change')))",
    ),
    (
        "workflows_session",
        "CREATE INDEX workflows_session ON workflows(session_id)",
    ),
    (
        "prompt_history",
        "CREATE TABLE prompt_history(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,position INTEGER NOT NULL CHECK(position>=0),prompt TEXT NOT NULL,PRIMARY KEY(session_id,position))",
    ),
    (
        "submissions",
        "CREATE TABLE submissions(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,command_id TEXT PRIMARY KEY,kind TEXT NOT NULL CHECK(kind IN('root_start','send_now_steer','host_wake')),queue_item_id TEXT NOT NULL,queue_revision INTEGER NOT NULL CHECK(queue_revision>0),target_turn_id TEXT,canonical_command TEXT NOT NULL,submitted_at_ms INTEGER NOT NULL CHECK(submitted_at_ms>=0),anchor INTEGER NOT NULL CHECK(anchor>=0))",
    ),
    (
        "submissions_session",
        "CREATE INDEX submissions_session ON submissions(session_id)",
    ),
    (
        "workflow_requests",
        "CREATE TABLE workflow_requests(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,operation_id TEXT PRIMARY KEY,source_locator TEXT NOT NULL,kind TEXT NOT NULL CHECK(kind IN('compact','clear','fork','rebind')),compact_reason TEXT,rebind_selection TEXT,anchor INTEGER NOT NULL CHECK(anchor>=0))",
    ),
    (
        "workflow_requests_session",
        "CREATE INDEX workflow_requests_session ON workflow_requests(session_id)",
    ),
    (
        "operator_notices",
        "CREATE TABLE operator_notices(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,notice_id TEXT NOT NULL,anchor INTEGER NOT NULL CHECK(anchor>=0),ordinal INTEGER NOT NULL CHECK(ordinal>=0),row TEXT NOT NULL CHECK(json_valid(row) AND json_type(row,'$.kind')='text'),PRIMARY KEY(session_id,notice_id),UNIQUE(session_id,anchor,ordinal))",
    ),
    (
        "host_intents",
        "CREATE TABLE host_intents(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,kind TEXT NOT NULL CHECK(kind IN('wake_suppression','pause_cause')),anchor INTEGER NOT NULL CHECK(anchor>=0),ordinal INTEGER NOT NULL CHECK(ordinal>=0),value TEXT NOT NULL CHECK(json_valid(value)),PRIMARY KEY(session_id,kind,anchor,ordinal))",
    ),
    (
        "projector_cursors",
        "CREATE TABLE projector_cursors(session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,runtime_session_id TEXT NOT NULL,applied_seq INTEGER NOT NULL CHECK(applied_seq>=0))",
    ),
    (
        "session_views",
        "CREATE TABLE session_views(session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,head_locator TEXT,root_turn_count INTEGER NOT NULL CHECK(root_turn_count>=0),tool_call_count INTEGER NOT NULL CHECK(tool_call_count>=0),compaction_count INTEGER NOT NULL CHECK(compaction_count>=0),state TEXT NOT NULL CHECK(json_valid(state) AND json_type(state,'$.calls') IS NULL AND json_type(state,'$.open_turns') IS NULL AND json_type(state,'$.processes') IS NULL))",
    ),
    (
        "session_call_views",
        "CREATE TABLE session_call_views(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,call_id TEXT NOT NULL,value TEXT NOT NULL CHECK(json_valid(value)),PRIMARY KEY(session_id,call_id))",
    ),
    (
        "session_open_turn_views",
        "CREATE TABLE session_open_turn_views(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,turn_id TEXT NOT NULL,value TEXT NOT NULL CHECK(json_valid(value)),PRIMARY KEY(session_id,turn_id))",
    ),
    (
        "session_process_views",
        "CREATE TABLE session_process_views(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,process_id INTEGER NOT NULL CHECK(process_id>=0),value TEXT NOT NULL CHECK(json_valid(value)),PRIMARY KEY(session_id,process_id))",
    ),
    (
        "transcript_rows",
        "CREATE TABLE transcript_rows(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,anchor INTEGER NOT NULL CHECK(anchor>=0),lane INTEGER NOT NULL CHECK(lane=0),ordinal INTEGER NOT NULL CHECK(ordinal>=0),row TEXT NOT NULL CHECK(json_valid(row) AND json_type(row,'$.kind')='text'),call_id TEXT,PRIMARY KEY(session_id,anchor,lane,ordinal))",
    ),
    (
        "transcript_rows_call",
        "CREATE UNIQUE INDEX transcript_rows_call ON transcript_rows(session_id,call_id) WHERE call_id IS NOT NULL",
    ),
    (
        "agent_acknowledgements",
        "CREATE TABLE agent_acknowledgements(tree_id TEXT NOT NULL,agent_id TEXT NOT NULL,attempt_id TEXT NOT NULL,attention TEXT NOT NULL,ended_at INTEGER,acknowledged_at INTEGER NOT NULL CHECK(acknowledged_at>=0),CHECK(ended_at IS NULL OR ended_at>=0),PRIMARY KEY(tree_id,agent_id,attempt_id,attention,ended_at))",
    ),
];

fn expected_schema_fingerprint() -> String {
    schema_fingerprint(SCHEMA_OBJECTS)
}

fn schema_fingerprint(objects: &[(&str, &str)]) -> String {
    digest_parts(
        &objects
            .iter()
            .flat_map(|(name, sql)| [*name, *sql])
            .collect::<Vec<_>>(),
    )
}

/// Operator acknowledgements of one attention occurrence.
pub mod acknowledgements;
#[cfg(feature = "test-support")]
pub(crate) mod benchmark;
/// The product's SQLite implementation of the hosted session-record port.
pub mod session_records;
/// The product session record's durable half: the free functions that open
/// `product.sqlite3` and the one-entry store cache behind them.
pub mod sessions;
/// The product's SQLite implementation of the tree store.
pub mod tree;
/// Schema 31 intents and the projector's views.
pub mod views;

fn validate_schema_shape(connection: &Connection) -> Result<(), String> {
    validate_schema_revision(connection, SCHEMA_OBJECTS)
}

fn validate_schema_revision(
    connection: &Connection,
    objects: &[(&str, &str)],
) -> Result<(), String> {
    let mut statement = connection.prepare(
        "SELECT name,sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' AND sql IS NOT NULL ORDER BY name",
    ).map_err(|_| "could not inspect product store shape".to_owned())?;
    let actual = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|_| "could not inspect product store shape".to_owned())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "product store shape is invalid".to_owned())?;
    let mut expected = objects
        .iter()
        .map(|(name, sql)| ((*name).to_owned(), (*sql).to_owned()))
        .collect::<Vec<_>>();
    expected.sort_by(|left, right| left.0.cmp(&right.0));
    if actual != expected {
        return Err("product store schema shape does not match its exact revision".to_owned());
    }
    let fingerprint: String = connection
        .query_row(
            "SELECT fingerprint FROM product_schema WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .map_err(|_| "product store schema fingerprint is missing".to_owned())?;
    if fingerprint != schema_fingerprint(objects) {
        return Err(
            "product store schema fingerprint does not match its exact revision".to_owned(),
        );
    }
    let integrity: String = connection
        .query_row("PRAGMA integrity_check(1)", [], |row| row.get(0))
        .map_err(|_| "could not inspect product store integrity".to_owned())?;
    if integrity != "ok" {
        return Err("product store integrity check failed".to_owned());
    }
    if connection
        .query_row("SELECT 1 FROM pragma_foreign_key_check LIMIT 1", [], |_| {
            Ok(())
        })
        .optional()
        .map_err(|_| "could not inspect product store foreign keys".to_owned())?
        .is_some()
    {
        return Err("product store foreign key check failed".to_owned());
    }
    Ok(())
}

/// One row of every type the product transcript stores, each with its
/// optional facts populated: a tool item with wait facts, every milestone
/// event, every notice tone. The session header is derived, never stored, and
/// the truncation marker is only ever the store's own first row.
#[cfg(test)]
pub(crate) fn every_stored_row_type() -> Vec<TranscriptRow> {
    use plakous_host_protocol::views::presentation::ActivityKind;
    use plakous_host_protocol::views::transcript::{
        AgentMilestone, AutoContinue, MilestoneEvent, ProcessNotice, ProcessOutcome, RetryItem,
        ThinkingRow, ToolItem, WaitFacts,
    };
    use plakous_hosted::sessions::NoticeScope;
    let milestone = |event| {
        TranscriptRow::Milestone(AgentMilestone {
            agent_id: "c39".into(),
            event,
            template: Some("worker".into()),
            text: Some("first line\nsecond".into()),
            duration_ms: Some(840_000),
            candidate: Some("cand-c39-a1".into()),
            files: Some(16),
            paths: vec!["src/lib.rs".into()],
        })
    };
    let mut rows = vec![
        TranscriptRow::user("go"),
        TranscriptRow::User {
            text: "go on".into(),
            sent_at_ms: Some(1_790_000_000_000),
        },
        TranscriptRow::steer("steer"),
        TranscriptRow::Steer {
            text: "steer again".into(),
            sent_at_ms: Some(1_790_000_060_000),
        },
        TranscriptRow::Thinking(ThinkingRow {
            location_key: "0:0".into(),
            text: "reason".into(),
            elapsed_ms: Some(2_100),
        }),
        TranscriptRow::Tool(ToolItem {
            call_id: "call-wait".into(),
            name: "wait_agent".into(),
            exposed_name: Some("wait".into()),
            kind: ActivityKind::Agent,
            state: "succeeded".into(),
            subject: Some("c39".into()),
            detail: Some("Result:\n{}".into()),
            started_at_ms: Some(1_000),
            finished_at_ms: Some(721_000),
            parent_call_id: Some("call-cell".into()),
            cell_id: Some("cell-1".into()),
            wait: Some(WaitFacts {
                timeout_ms: Some(3_600_000),
                woke_by: Some("c39 finished".into()),
                waited_ms: Some(720_000),
                settled_agent: Some("c39".into()),
            }),
        }),
        TranscriptRow::Process(ProcessNotice {
            job_id: "12".into(),
            outcome: ProcessOutcome::Exited { code: Some(2) },
            duration_ms: Some(62_000),
            subject: Some("just check".into()),
        }),
        TranscriptRow::Process(ProcessNotice {
            job_id: "13".into(),
            outcome: ProcessOutcome::Stopped,
            duration_ms: None,
            subject: None,
        }),
        TranscriptRow::Retry(RetryItem {
            attempt: 2,
            max_attempts: 4,
            cause: "429 rate limited".into(),
        }),
        TranscriptRow::AutoContinue(AutoContinue {
            agents_finished: 2,
            processes_exited: 1,
            agents: vec!["c38".into(), "c39".into()],
        }),
        TranscriptRow::info(NoticeScope::Tree, "note"),
        TranscriptRow::warning(NoticeScope::Settings, "warning"),
        TranscriptRow::error(NoticeScope::Turn, "error"),
        TranscriptRow::info(NoticeScope::Diagnostic, "History projection x: y"),
        TranscriptRow::assistant("done"),
        TranscriptRow::Interrupted,
    ];
    rows.extend(
        [
            MilestoneEvent::Spawned,
            MilestoneEvent::Retasked,
            MilestoneEvent::Finished,
            MilestoneEvent::Failed,
            MilestoneEvent::NeedsRecovery,
            MilestoneEvent::Asks,
            MilestoneEvent::Adopted,
            MilestoneEvent::AdoptConflict,
        ]
        .map(milestone),
    );
    rows
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests {
    use super::*;
    use plakous_hosted::sessions::NoticeScope;
    use plakous_hosted::sessions::projector::{
        Delivery, SessionViews, Submission, SubmissionKind, ViewWrites,
    };
    use tempfile::tempdir;

    #[cfg(feature = "test-hooks")]
    #[test]
    fn commit_trace_frameless_product_ops_release_for_another_thread() {
        use plakous_offline_crash_harness::commit_trace::{KILL_ENV, TRACE_ENV};
        for (mode, expected) in [("acknowledge", 3), ("delete", 2)] {
            let root = tempdir().unwrap();
            let trace = root.path().join("trace.jsonl");
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "store::tests::commit_trace_frameless_product_child",
                    "--nocapture",
                ])
                .env("PLAK_SW1_PRODUCT_FRAMELESS_ROOT", root.path())
                .env("PLAK_SW1_PRODUCT_FRAMELESS_MODE", mode)
                .env(TRACE_ENV, &trace)
                .env_remove(KILL_ENV)
                .output()
                .unwrap();
            assert!(output.status.success(), "{mode}: {output:?}");
            let lines: Vec<serde_json::Value> = std::fs::read_to_string(trace)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(lines.len(), expected);
            for (index, line) in lines.iter().enumerate() {
                assert_eq!(line["store"], "product");
                assert_eq!(line["ordinal"], index + 1);
            }
        }
    }

    #[cfg(feature = "test-hooks")]
    #[test]
    fn commit_trace_frameless_product_child() {
        let Some(root) = std::env::var_os("PLAK_SW1_PRODUCT_FRAMELESS_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let mut store = ProductStore::open(&PlakPaths::explicit(root.join("p"))).unwrap();
        if std::env::var("PLAK_SW1_PRODUCT_FRAMELESS_MODE").unwrap() == "acknowledge" {
            let ack = crate::surface_protocol::AgentAcknowledgement {
                agent_id: "agent".to_owned(),
                attempt_id: "attempt".to_owned(),
                attention: "failed".to_owned(),
                ended_at: Some(10),
            };
            store.acknowledge_agent_attempt("tree", &ack, 10).unwrap();
            store.acknowledge_agent_attempt("tree", &ack, 20).unwrap();
        } else {
            store.delete_agent_acknowledgements("absent").unwrap();
        }
        std::thread::spawn(move || {
            let mut peer = ProductStore::open(&PlakPaths::explicit(root.join("p"))).unwrap();
            peer.commit(&mut SessionRecord::new(root)).unwrap();
        })
        .join()
        .unwrap();
    }

    #[cfg(feature = "test-hooks")]
    #[test]
    fn commit_trace_product_and_tree_share_one_process_sequence() {
        use plakous_offline_crash_harness::commit_trace::{KILL_ENV, TRACE_ENV};
        let root = tempdir().unwrap();
        let trace = root.path().join("trace.jsonl");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "store::tests::commit_trace_product_and_tree_child",
                "--nocapture",
            ])
            .env("PLAK_SW1_PRODUCT_TRACE_ROOT", root.path())
            .env(TRACE_ENV, &trace)
            .env_remove(KILL_ENV)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let lines: Vec<serde_json::Value> = std::fs::read_to_string(trace)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(lines.iter().any(|line| line["store"] == "product"));
        assert!(lines.iter().any(|line| line["store"] == "tree"));
        assert!(lines.iter().any(|line| {
            line["tables"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("sessions"))
        }));
        for (index, line) in lines.iter().enumerate() {
            assert_eq!(line["ordinal"], index + 1);
            assert_eq!(line["indexed"], true);
        }
    }

    #[cfg(feature = "test-hooks")]
    #[test]
    fn commit_trace_product_and_tree_child() {
        let Some(root) = std::env::var_os("PLAK_SW1_PRODUCT_TRACE_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let paths = PlakPaths::explicit(root.join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut session = SessionRecord::new(root.clone());
        store.commit(&mut session).unwrap();
        let _tree =
            plakous_recursive_subagents_store::SqliteTreeStore::open(&root.join("tree.sqlite3"))
                .unwrap();
        session.append_transcript(TranscriptRow::user("trace fixture"));
        store.commit(&mut session).unwrap();
    }

    /// Commits `rows` as `session_id`'s projector rows (anchor 1, lane 0),
    /// replacing any views it had: the store side of a projector run.
    fn project_rows(store: &mut ProductStore, session_id: &str, rows: &[TranscriptRow]) {
        use plakous_hosted::sessions::projector::{Lane, RowKey, SessionViews, ViewWrites};
        store.delete_views(session_id).unwrap();
        let mut views = SessionViews::empty();
        views.cursor = plakous::sessions::feed::FeedSeq::new(1);
        for (ordinal, row) in rows.iter().enumerate() {
            views.rows.insert(
                RowKey::new(1, Lane::Runtime, u32::try_from(ordinal).unwrap()),
                row.clone(),
            );
        }
        let writes = ViewWrites {
            expected_cursor: plakous::sessions::feed::FeedSeq::ZERO,
            cursor: views.cursor,
            upserts: views
                .rows
                .iter()
                .map(|(key, row)| (*key, row.clone()))
                .collect(),
            deletes: Vec::new(),
            calls: Vec::new(),
            open_turns: Vec::new(),
            processes: Vec::new(),
            successors: Vec::new(),
        };
        assert_eq!(
            store
                .commit_projection_views(session_id, &writes, &views)
                .unwrap(),
            plakous_hosted::sessions::projector::ProjectionCommit::Applied
        );
    }

    /// Every stored row type, with every optional fact populated, reloads as
    /// the same typed row from a fresh store open: through the projector's
    /// view commit for runtime rows and as operator notice intents.
    #[test]
    fn every_transcript_row_type_round_trips_through_the_store() {
        use plakous_hosted::sessions::projector::{Lane, RowKey, SessionViews, ViewWrites};
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut session = SessionRecord::new(root.path().to_path_buf());
        let rows = every_stored_row_type();
        let mut views = SessionViews::empty();
        views.cursor = plakous::sessions::feed::FeedSeq::new(1);
        for (ordinal, row) in rows.iter().enumerate() {
            views.rows.insert(
                RowKey::new(1, Lane::Runtime, u32::try_from(ordinal).unwrap()),
                row.clone(),
            );
        }
        {
            let mut store = ProductStore::open(&paths).unwrap();
            store.commit(&mut session).unwrap();
            let writes = ViewWrites {
                expected_cursor: plakous::sessions::feed::FeedSeq::ZERO,
                cursor: views.cursor,
                upserts: views
                    .rows
                    .iter()
                    .map(|(key, row)| (*key, row.clone()))
                    .collect(),
                deletes: Vec::new(),
                calls: Vec::new(),
                open_turns: Vec::new(),
                processes: Vec::new(),
                successors: Vec::new(),
            };
            store
                .commit_projection_views(&session.id, &writes, &views)
                .unwrap();
            session.record_notice(TranscriptRow::warning(NoticeScope::Tree, "a notice intent"));
            store.commit(&mut session).unwrap();
        }
        let loaded = ProductStore::open(&paths)
            .unwrap()
            .load(&session.id)
            .unwrap();
        let mut expected = rows;
        expected.push(TranscriptRow::warning(NoticeScope::Tree, "a notice intent"));
        assert_eq!(
            loaded.transcript.iter().cloned().collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn a_held_product_writer_refuses_without_partial_binding_and_retries_after_release() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("home"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut writer = Connection::open(paths.product_database()).unwrap();
        let transaction = writer
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        store.connection.busy_timeout(Duration::ZERO).unwrap();
        let mut session = SessionRecord::new(root.path().join("workspace"));
        assert_eq!(
            store.commit(&mut session),
            Err("product store is busy".to_owned())
        );
        assert_eq!(session.store_revision, 0);
        assert!(store.load(&session.id).is_err());
        let bindings: i64 = transaction
            .query_row("SELECT COUNT(*) FROM identity_bindings", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(bindings, 0);
        transaction.rollback().unwrap();
        store.commit(&mut session).unwrap();
        assert_eq!(session.store_revision, 1);
        assert_eq!(store.load(&session.id).unwrap().id, session.id);
    }

    #[test]
    fn a_held_product_writer_refuses_a_submission_as_typed_busy() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("home"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut session = SessionRecord::new(root.path().join("workspace"));
        store.commit(&mut session).unwrap();
        let item = QueueItem::new("wake");
        session.pending_queue.push(item.clone());
        let request = submission(&item, "wake-command");
        let mut writer = Connection::open(paths.product_database()).unwrap();
        let transaction = writer
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        store.connection.busy_timeout(Duration::ZERO).unwrap();
        let before = session.clone();
        let refused = store.commit_submission(&mut session, &request).unwrap_err();
        assert_eq!(refused.refusal(), Some(StoreRefusal::Busy));
        assert_eq!(refused.message(), "product store is busy");
        assert_eq!(session, before);
        transaction.rollback().unwrap();
        store.commit_submission(&mut session, &request).unwrap();
        assert_eq!(
            store.load_intents(&session.id).unwrap().submissions,
            [request]
        );
    }

    #[test]
    fn read_only_session_inspection_sees_live_wal_without_creating_or_writing_state() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("home"));
        let unknown = "00000000-0000-4000-8000-000000000001";
        assert!(ProductStore::inspect_session_read_only(&paths, unknown).is_err());
        assert!(!paths.product_database().exists());
        assert!(!root.path().join("home").exists());

        let mut store = ProductStore::open(&paths).unwrap();
        let mut session = SessionRecord::new(root.path().join("workspace"));
        session.account = Some("codex".to_owned());
        store.commit(&mut session).unwrap();
        let wal = PathBuf::from(format!("{}-wal", paths.product_database().display()));
        assert!(wal.metadata().unwrap().len() > 0);
        let before_version: i64 = store
            .connection
            .query_row("PRAGMA schema_version", [], |row| row.get(0))
            .unwrap();
        let before_fingerprint: String = store
            .connection
            .query_row(
                "SELECT fingerprint FROM product_schema WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let before_revision = session.store_revision;
        let inspected = ProductStore::inspect_session_read_only(&paths, &session.id).unwrap();
        assert_eq!(inspected.id, session.id);
        assert_eq!(inspected.account, session.account);
        assert_eq!(inspected.store_revision, before_revision);
        assert_eq!(
            store.load(&session.id).unwrap().store_revision,
            before_revision
        );
        assert_eq!(
            store
                .connection
                .query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            before_version
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT fingerprint FROM product_schema WHERE singleton=1",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            before_fingerprint
        );
    }

    fn assert_exact_schema(paths: &PlakPaths) {
        let connection = Connection::open(paths.product_database()).unwrap();
        validate_schema_revision(&connection, SCHEMA_OBJECTS).unwrap();
        assert_eq!(
            connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
    }

    fn submission(item: &QueueItem, command: &str) -> Submission {
        Submission {
            command_id: command.into(),
            kind: SubmissionKind::RootStart,
            queue_item_id: item.id.clone(),
            queue_item_revision: item.revision,
            target_turn_id: None,
            canonical_command: item.text.clone(),
            submitted_at_ms: item.submitted_at_ms,
        }
    }

    fn project_delivery(store: &mut ProductStore, id: &str, command: &str, delivery: Delivery) {
        let before = store
            .load_views(id)
            .unwrap()
            .unwrap_or_else(SessionViews::empty);
        let mut views = before.clone();
        views.state.deliveries.insert(command.into(), delivery);
        store
            .commit_projection_views(id, &ViewWrites::between(&before, &views), &views)
            .unwrap();
    }

    fn append_settled_submission(
        store: &mut ProductStore,
        session: &mut SessionRecord,
        index: usize,
    ) -> Submission {
        let item = QueueItem::new(format!("prompt-{index}"));
        let request = submission(&item, &format!("plak.command.{index}"));
        session.pending_queue.push(item);
        store.commit_submission(session, &request).unwrap();
        project_delivery(store, &session.id, &request.command_id, Delivery::Accepted);
        session.pending_queue.clear();
        store
            .discard_submission(session, &request.command_id, None)
            .unwrap();
        request
    }

    #[test]
    fn exact_schema_wal_and_cas() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        assert_exact_schema(&paths);
        let mut session = SessionRecord::new(root.path().to_path_buf());
        store.commit(&mut session).unwrap();
        assert_eq!(session.store_revision, 1);
        let mut stale = session.clone();
        store.commit(&mut session).unwrap();
        assert_eq!(
            store.commit(&mut stale).unwrap_err(),
            "product session changed concurrently"
        );
        let c = Connection::open(paths.product_database()).unwrap();
        assert_eq!(
            c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
        assert_eq!(
            c.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "wal"
        );
    }

    #[test]
    fn target_selection_round_trips_native_and_openrouter() {
        use crate::product_config::{
            ProductCompositionSelection, ProductOpenRouterRoutingSelection,
            ProductProtocolSelection, ProductWireBinding,
        };

        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut session = SessionRecord::new(root.path().join("workspace"));
        session.target_selection = ProductTargetSelection::OpenRouter {
            wire: ProductWireBinding::resolve(ProductProtocolSelection::Auto),
            routing: ProductOpenRouterRoutingSelection::GatewayManaged,
        };
        session.account = Some("openrouter".into());
        session.model = Some("fixture/openrouter-model".into());
        session.effort = Some("medium".into());
        session.composition = ProductCompositionSelection::Direct;
        let id = session.id.clone();
        store.commit(&mut session).unwrap();
        drop(store);

        let mut reopened = ProductStore::open(&paths).unwrap();
        let loaded = reopened.load(&id).unwrap();
        assert_eq!(loaded.target_selection, session.target_selection);
        assert_eq!(
            reopened
                .list_for_workspace(&session.workspace_root)
                .unwrap()[0]
                .target_selection,
            session.target_selection
        );
    }

    /// The root's frozen tools are written once: a save without them, or
    /// with others, keeps what the first creation froze.
    #[test]
    fn frozen_root_tools_round_trip_and_are_written_once() {
        use plakous_hosted::binding::RootTemplateFact;
        use plakous_hosted::frozen_tools::FrozenRootTools;
        use plakous_hosted::tool_plan::TemplateToolSelection;

        let fact = |tools: &[&str]| RootTemplateFact {
            name: "root-agent".into(),
            account: "codex".into(),
            model: "m".into(),
            tools: TemplateToolSelection::parse(tools).unwrap(),
            tool_sections: std::collections::BTreeMap::from([(
                "tools.fetch_url".to_owned(),
                "[tools.fetch_url]\nmax_bytes = 4096\n".to_owned(),
            )]),
        };
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut session = SessionRecord::new(root.path().join("workspace"));
        let id = session.id.clone();
        store.commit(&mut session).unwrap();
        assert_eq!(store.load(&id).unwrap().frozen_tools, None);

        let frozen = FrozenRootTools::new(Some(fact(&["fetch_url"]))).with_mcp_tools(Vec::new());
        session.frozen_tools = Some(frozen.clone());
        store.commit(&mut session).unwrap();
        session.frozen_tools = Some(FrozenRootTools::new(Some(fact(&["image_gen"]))));
        store.commit(&mut session).unwrap();
        session.frozen_tools = None;
        store.commit(&mut session).unwrap();
        drop(store);

        let mut reopened = ProductStore::open(&paths).unwrap();
        assert_eq!(reopened.load(&id).unwrap().frozen_tools, Some(frozen));
    }

    /// The resume picker's summary is the session's *opening* prompt, and its
    /// age column is the record's own timestamps. A listing projects all three
    /// even though `transcript` carries the most recent prompts instead.
    #[test]
    fn a_workspace_listing_projects_the_first_prompt_and_both_timestamps() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        let workspace = root.path().join("workspace");

        let mut older = SessionRecord::new(workspace.clone());
        older.created_at_unix = 1_788_000_000;
        older.updated_at_unix = 1_788_100_000;
        store.commit(&mut older).unwrap();
        project_rows(
            &mut store,
            &older.id,
            &[
                TranscriptRow::user("the very first thing asked"),
                TranscriptRow::assistant("an answer"),
                TranscriptRow::user("the most recent thing asked"),
            ],
        );

        let mut newer = SessionRecord::new(workspace.clone());
        newer.created_at_unix = 1_788_200_000;
        newer.updated_at_unix = 1_788_300_000;
        store.commit(&mut newer).unwrap();
        project_rows(
            &mut store,
            &newer.id,
            &[TranscriptRow::assistant("nobody prompted this one")],
        );

        let listed = store.list_for_workspace(&workspace).unwrap();
        assert_eq!(
            listed.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            [newer.id.as_str(), older.id.as_str()],
            "newest first"
        );
        assert_eq!(listed[0].created_at_unix, 1_788_200_000);
        assert_eq!(listed[0].updated_at_unix, 1_788_300_000);
        assert_eq!(
            listed[0].first_prompt, None,
            "a session with no user line projects no first prompt"
        );
        assert_eq!(listed[1].created_at_unix, 1_788_000_000);
        assert_eq!(listed[1].updated_at_unix, 1_788_100_000);
        assert_eq!(
            listed[1].first_prompt.as_deref(),
            Some("the very first thing asked"),
            "the projection is the opening prompt, not the latest one"
        );
        assert!(
            listed[1].transcript.iter().any(|line| line
                .text()
                .is_some_and(|text| text.contains("the most recent thing asked"))),
            "and the recent-prompt projection is untouched"
        );

        // Loading one record is not a listing: the projection stays absent
        // rather than being half-filled from a different query.
        assert_eq!(store.load(&older.id).unwrap().first_prompt, None);
    }

    /// A first prompt longer than the picker's bound is cut at the store, so a
    /// pasted wall of text never reaches a one-line row.
    #[test]
    fn a_projected_first_prompt_is_bounded_to_the_pickers_column() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        let workspace = root.path().join("workspace");
        let mut session = SessionRecord::new(workspace.clone());
        store.commit(&mut session).unwrap();
        project_rows(
            &mut store,
            &session.id,
            &[TranscriptRow::user("\u{e9}".repeat(500))],
        );

        let listed = store.list_for_workspace(&workspace).unwrap();
        let projected = listed[0].first_prompt.as_deref().unwrap();
        assert_eq!(
            projected.chars().count(),
            MAX_LIST_FIRST_PROMPT_CHARS,
            "the bound counts characters, not bytes"
        );
    }

    #[test]
    fn child_wake_cancel_suppression_round_trips() {
        let dir = tempdir().unwrap();
        let paths = PlakPaths::explicit(dir.path().join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut session = SessionRecord::new(dir.path().to_owned());
        store.commit(&mut session).unwrap();
        store.write_host_controls(&session.id, true, None).unwrap();
        drop(store);
        let mut store = ProductStore::open(&paths).unwrap();
        let saved = store.load(&session.id).unwrap();
        assert!(saved.child_wake_suppressed);
    }

    #[test]
    fn prompt_history_round_trips_across_store_restart_without_transcript_reconstruction() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut session = SessionRecord::new(root.path().to_path_buf());
        session.record_prompt("first".into());
        session.record_prompt("second".into());
        let id = session.id.clone();
        {
            let mut store = ProductStore::open(&paths).unwrap();
            store.commit(&mut session).unwrap();
            project_rows(&mut store, &id, &[TranscriptRow::user("transcript-only")]);
        }
        let mut restarted = ProductStore::open(&paths).unwrap();
        let loaded = restarted.load(&id).unwrap();
        assert_eq!(loaded.prompt_history, ["first", "second"]);
        assert_eq!(
            loaded.transcript,
            vec![TranscriptRow::user("transcript-only")]
        );
    }

    #[test]
    fn prompt_history_append_and_front_prune_touch_constant_rows() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        for index in 0..sessions::MAX_PROMPT_HISTORY {
            session.record_prompt(format!("prompt-{index}"));
        }
        store.commit(&mut session).unwrap();
        store.connection.execute_batch(
            "CREATE TEMP TABLE prompt_work(inserted INTEGER NOT NULL,deleted INTEGER NOT NULL);
             INSERT INTO prompt_work VALUES(0,0);
             CREATE TEMP TRIGGER count_prompt_insert AFTER INSERT ON prompt_history BEGIN UPDATE prompt_work SET inserted=inserted+1; END;
             CREATE TEMP TRIGGER count_prompt_delete AFTER DELETE ON prompt_history BEGIN UPDATE prompt_work SET deleted=deleted+1; END;",
        ).unwrap();
        session.record_prompt("prompt-500".into());
        store.commit(&mut session).unwrap();
        assert_eq!(
            store
                .connection
                .query_row("SELECT inserted,deleted FROM prompt_work", [], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                })
                .unwrap(),
            (1, 1)
        );
    }
    #[test]
    fn unknown_schema_is_refused() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        paths.ensure_layout().unwrap();
        let c = Connection::open(paths.product_database()).unwrap();
        c.pragma_update(None, "user_version", 99).unwrap();
        drop(c);
        let error = ProductStore::open(&paths).unwrap_err();
        assert_eq!(
            error.kind(),
            ProductStoreOpenErrorKind::UnsupportedPersistence
        );
        assert!(
            error
                .to_string()
                .contains("created by an incompatible plak build")
        );
    }

    #[test]
    fn current_schema_rejects_previous_product_store_without_modification() {
        // A product schema change moves the state directory; a store at
        // another version in this directory is refused with its rows intact.
        assert_eq!(SCHEMA_VERSION, 32);
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        paths.ensure_layout().unwrap();
        {
            let mut store = ProductStore::open(&paths).unwrap();
            let mut session = SessionRecord::new(root.path().join("workspace"));
            store.commit(&mut session).unwrap();
        }
        let connection = Connection::open(paths.product_database()).unwrap();
        connection.pragma_update(None, "user_version", 31).unwrap();
        drop(connection);

        let error = ProductStore::open(&paths).unwrap_err();
        assert_eq!(
            error.kind(),
            ProductStoreOpenErrorKind::UnsupportedPersistence
        );
        assert!(
            error.to_string().contains("product schema 31, expected 32"),
            "{error}"
        );
        assert!(
            error
                .to_string()
                .contains("created by an incompatible plak build"),
            "{error}"
        );
        // Fail-closed means the rows are still there, untouched.
        let connection = Connection::open(paths.product_database()).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM sessions", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn workspace_listing_uses_two_set_queries_and_preserves_bounded_previews() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let workspace = root.path().join("workspace");
        let mut store = ProductStore::open(&paths).unwrap();

        let mut prompted = SessionRecord::new(workspace.clone());
        store.commit(&mut prompted).unwrap();
        project_rows(
            &mut store,
            &prompted.id,
            &[
                TranscriptRow::assistant("older answer"),
                TranscriptRow::user("first prompt"),
                TranscriptRow::assistant("newer answer"),
                TranscriptRow::user("second prompt"),
            ],
        );
        let mut fallback = SessionRecord::new(workspace.clone());
        store.commit(&mut fallback).unwrap();
        project_rows(
            &mut store,
            &fallback.id,
            &[
                TranscriptRow::error(NoticeScope::Turn, "ignored"),
                TranscriptRow::assistant("first usable line"),
                TranscriptRow::assistant("later line"),
            ],
        );
        let mut empty = SessionRecord::new(workspace.clone());
        store.commit(&mut empty).unwrap();

        let (listed, query_count) = store
            .list_for_workspace_with_query_count(&workspace)
            .unwrap();
        assert_eq!(
            query_count, 2,
            "picker latency must not grow by SQL round trips"
        );
        assert_eq!(listed.len(), 3);
        let prompted = listed.iter().find(|row| row.id == prompted.id).unwrap();
        assert_eq!(
            prompted.transcript,
            vec![
                TranscriptRow::user("first prompt"),
                TranscriptRow::user("second prompt")
            ]
        );
        let fallback = listed.iter().find(|row| row.id == fallback.id).unwrap();
        assert_eq!(
            fallback.transcript,
            vec![TranscriptRow::assistant("first usable line")]
        );
        let empty = listed.iter().find(|row| row.id == empty.id).unwrap();
        assert!(empty.transcript.is_empty());
    }

    #[test]
    fn corrupt_database_is_unavailable_not_unsupported_persistence() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        paths.ensure_layout().unwrap();
        std::fs::write(paths.product_database(), b"not a sqlite database").unwrap();

        let error = ProductStore::open(&paths).unwrap_err();

        assert_eq!(error.kind(), ProductStoreOpenErrorKind::Unavailable);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_product_database_is_refused_without_touching_target() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        paths.ensure_layout().unwrap();
        let external = root.path().join("external.sqlite3");
        std::fs::write(&external, b"external sentinel").unwrap();
        symlink(&external, paths.product_database()).unwrap();

        let error = ProductStore::open(&paths).unwrap_err();

        assert_eq!(error.kind(), ProductStoreOpenErrorKind::Unavailable);
        assert_eq!(std::fs::read(external).unwrap(), b"external sentinel");
    }

    #[test]
    fn global_registry_terminal_domain_and_exact_transition_are_closed() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        for (kind, state) in [
            ("create", "rejected"),
            ("delivery", "terminal_no_change"),
            ("workflow", "rejected"),
        ] {
            assert!(store.connection.execute("INSERT INTO identity_bindings(identity,kind,owner_session_id,binding_digest,terminal_disposition) VALUES(?1,?2,'owner','digest',?3)",params![format!("bad:{kind}"),kind,state]).is_err());
        }
        let tx = store.connection.transaction().unwrap();
        bind_identity(&tx, "delivery", "operation", "owner", "digest", None, None).unwrap();
        bind_identity(
            &tx,
            "delivery",
            "operation",
            "owner",
            "digest",
            Some("product_committed"),
            None,
        )
        .unwrap();
        bind_identity(
            &tx,
            "delivery",
            "operation",
            "owner",
            "digest",
            Some("product_committed"),
            None,
        )
        .unwrap();
        tx.commit().unwrap();
        let tx = store.connection.transaction().unwrap();
        assert!(
            bind_identity(&tx, "delivery", "operation", "owner", "digest", None, None).is_err()
        );
        drop(tx);
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT terminal_disposition FROM identity_bindings WHERE identity='operation'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "product_committed"
        );
    }

    #[test]
    fn unrecorded_refusal_releases_submission_but_preserves_exact_retry_binding() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut session = SessionRecord::new(root.path().to_owned());
        let item = QueueItem::new("retry");
        session.pending_queue.push(item.clone());
        let request = submission(&item, "retry-command");
        for _ in 0..2 {
            store.commit_submission(&mut session, &request).unwrap();
            store
                .discard_submission(&mut session, &request.command_id, None)
                .unwrap();
            session = store.load(&session.id).unwrap();
            assert_eq!(
                store.load_intents(&session.id).unwrap().submissions,
                [] as [Submission; 0]
            );
            assert_eq!(session.pending_queue, vec![item.clone()]);
            let bindings: i64 = store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM identity_bindings WHERE identity=?1",
                    [request.binding_identity()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(bindings, 1);
            let conflict = Submission {
                canonical_command: "changed".into(),
                ..request.clone()
            };
            assert!(store.commit_submission(&mut session, &conflict).is_err());
        }
    }

    #[test]
    fn result_mail_command_roundtrips_and_is_part_of_immutable_submission_identity() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut session = SessionRecord::new(root.path().to_owned());
        let item = QueueItem::new("continue");
        session.pending_queue.push(item.clone());
        store.commit(&mut session).unwrap();
        let mut request = submission(&item, "result-command");
        request.canonical_command = "continue with exact result m1".into();
        let mut competing = session.clone();
        let changed = Submission {
            canonical_command: "continue with exact result m2".into(),
            ..request.clone()
        };
        let mut second_handle = ProductStore::open(&paths).unwrap();
        store.commit_submission(&mut session, &request).unwrap();
        assert!(
            second_handle
                .commit_submission(&mut competing, &changed)
                .is_err()
        );
        assert_eq!(
            store.load_intents(&session.id).unwrap().submissions,
            vec![request.clone()]
        );
        let mut loaded = store.load(&session.id).unwrap();
        assert!(store.commit_submission(&mut loaded, &changed).is_err());
        assert_eq!(
            store.load_intents(&session.id).unwrap().submissions,
            vec![request]
        );
    }

    #[test]
    fn reading_settled_submissions_does_not_change_the_next_commit_writes() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        let item = QueueItem::new("command body stays immutable");
        session.pending_queue.push(item.clone());
        let request = submission(&item, "settled-command");
        store.commit_submission(&mut session, &request).unwrap();
        project_delivery(
            &mut store,
            &session.id,
            &request.command_id,
            Delivery::Accepted,
        );
        store.connection.execute_batch("CREATE TEMP TABLE submission_writes(command_id TEXT NOT NULL); CREATE TEMP TRIGGER observe_submission_update AFTER UPDATE ON submissions BEGIN INSERT INTO submission_writes VALUES(NEW.command_id); END;").unwrap();
        assert_eq!(
            store.load(&session.id).unwrap().pending_queue,
            [] as [QueueItem; 0]
        );
        assert_eq!(
            store.load_intents(&session.id).unwrap().submissions,
            vec![request]
        );
        store.commit(&mut session).unwrap();
        let writes: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM submission_writes", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            writes, 0,
            "a read changed the next intent commit's write shape"
        );
    }

    #[test]
    fn pending_rejection_survives_submission_settlement_and_reopen() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        let item = QueueItem::new("rejected");
        session.pending_queue.push(item.clone());
        let request = submission(&item, "rejected-command");
        store.commit_submission(&mut session, &request).unwrap();
        project_delivery(
            &mut store,
            &session.id,
            &request.command_id,
            Delivery::Rejected {
                error: "refused".into(),
            },
        );
        // The pending rejected revision must survive unrelated settled commands.
        for index in 0..129 {
            let next = QueueItem::new(format!("prompt-{index}"));
            let next_request = submission(&next, &format!("command-{index}"));
            session.pending_queue.push(next.clone());
            store
                .commit_submission(&mut session, &next_request)
                .unwrap();
            project_delivery(
                &mut store,
                &session.id,
                &next_request.command_id,
                Delivery::Accepted,
            );
            session.pending_queue.retain(|item| item.id != next.id);
            store
                .discard_submission(&mut session, &next_request.command_id, None)
                .unwrap();
        }
        drop(store);
        let mut store = ProductStore::open(&paths).unwrap();
        let loaded = store.load(&session.id).unwrap();
        assert_eq!(loaded.pending_queue, vec![item.clone()]);
        assert!(loaded.queue_commands.is_rejected(&item));
        assert!(!loaded.queue_commands.is_locked(&item.id));
        assert_eq!(
            store.load_intents(&session.id).unwrap().submissions,
            vec![request]
        );
    }

    #[test]
    fn lifetime_submission_4097_removes_detail_but_preserves_exact_retry_binding() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        let first = append_settled_submission(&mut store, &mut session, 0);
        let tx = store.connection.transaction().unwrap();
        for index in 1..4096 {
            let request = submission(
                &QueueItem::new(format!("prompt-{index}")),
                &format!("command-{index}"),
            );
            bind_identity(
                &tx,
                "command",
                &request.binding_identity(),
                &session.id,
                &request.binding_digest(&session.id),
                None,
                None,
            )
            .unwrap();
        }
        tx.commit().unwrap();
        append_settled_submission(&mut store, &mut session, 4096);
        let detail: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM submissions", [], |row| row.get(0))
            .unwrap();
        let bindings: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM identity_bindings WHERE kind='command'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(detail, 0);
        assert_eq!(bindings, 4097);
        let conflict = Submission {
            canonical_command: "changed prompt-0".into(),
            ..first.clone()
        };
        assert!(store.commit_submission(&mut session, &conflict).is_err());
        store.write_submission(&session.id, &first).unwrap();
        store.write_submission(&session.id, &first).unwrap();
        assert_eq!(
            store.load_intents(&session.id).unwrap().submissions,
            vec![first]
        );
    }

    #[test]
    fn submission_withdrawal_failure_rolls_back_registry_and_parent() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        let item = QueueItem::new("rollback command");
        session.pending_queue.push(item.clone());
        let request = submission(&item, "rollback-command");
        store.commit_submission(&mut session, &request).unwrap();
        let before = store.load(&session.id).unwrap();
        let revision = session.store_revision;
        store.connection.execute_batch("CREATE TEMP TRIGGER fail_submission_withdrawal BEFORE DELETE ON submissions BEGIN SELECT RAISE(ABORT,'cut'); END;").unwrap();
        session.pending_queue.clear();
        assert!(
            store
                .discard_submission(&mut session, &request.command_id, None)
                .is_err()
        );
        assert_eq!(session.store_revision, revision);
        assert_eq!(store.load(&session.id).unwrap(), before);
        assert_eq!(
            store.load_intents(&session.id).unwrap().submissions,
            vec![request.clone()]
        );
        let disposition: Option<String> = store
            .connection
            .query_row(
                "SELECT terminal_disposition FROM identity_bindings WHERE identity=?1",
                [&item.id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(disposition.is_none());
        assert!(
            store
                .write_submission(
                    &session.id,
                    &Submission {
                        canonical_command: "changed".into(),
                        ..request
                    }
                )
                .is_err()
        );
    }

    #[test]
    fn unresolved_submission_survives_unrelated_settlements() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        for index in 0..129 {
            append_settled_submission(&mut store, &mut session, index);
        }
        let item = QueueItem::new("still unresolved");
        session.pending_queue.push(item.clone());
        let request = submission(&item, "unresolved-command");
        store.commit_submission(&mut session, &request).unwrap();
        store.commit(&mut session).unwrap();
        assert_eq!(
            store.load_intents(&session.id).unwrap().submissions,
            vec![request]
        );
        assert!(
            store
                .load(&session.id)
                .unwrap()
                .queue_commands
                .is_locked(&item.id)
        );
    }

    #[test]
    fn user_queue_delete_seals_tombstone_and_forbids_resurrection() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        let item = QueueItem::new("delete me");
        session.pending_queue.push(item.clone());
        store.commit(&mut session).unwrap();
        session.pending_queue.clear();
        store.commit(&mut session).unwrap();
        assert_eq!(
            store.load(&session.id).unwrap().pending_queue,
            [] as [QueueItem; 0]
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT terminal_disposition FROM identity_bindings WHERE identity=?1",
                    [&item.id],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "deleted"
        );
        session.pending_queue.push(item);
        assert!(store.commit(&mut session).is_err());
    }

    #[test]
    fn accepted_submission_consumes_only_its_revision_before_queue_cleanup() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        let item = QueueItem::new("consume me");
        session.pending_queue.push(item.clone());
        let request = submission(&item, "consume-command");
        store.commit_submission(&mut session, &request).unwrap();
        project_delivery(
            &mut store,
            &session.id,
            &request.command_id,
            Delivery::Accepted,
        );
        assert_eq!(
            store.load(&session.id).unwrap().pending_queue,
            [] as [QueueItem; 0]
        );
        let retained: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM queue_items WHERE session_id=?1",
                [&session.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained, 1, "projection does not rewrite queue intents");
        session.pending_queue.clear();
        store.commit(&mut session).unwrap();
        session.pending_queue.push(item);
        assert!(store.commit(&mut session).is_err());
    }

    #[test]
    fn projected_queue_reads_do_not_suppress_the_consuming_intent_write() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        let item = QueueItem::new("consume me");
        session.pending_queue.push(item.clone());
        let request = submission(&item, "projected-consume");
        store.commit_submission(&mut session, &request).unwrap();
        project_delivery(
            &mut store,
            &session.id,
            &request.command_id,
            Delivery::Accepted,
        );
        assert_eq!(
            store.load(&session.id).unwrap().pending_queue,
            [] as [QueueItem; 0]
        );
        session.pending_queue.clear();
        store.commit(&mut session).unwrap();
        let remaining: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM queue_items WHERE session_id=?1",
                [&session.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn queue_edit_requires_exact_replay_or_atomic_revision_increment() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        session.pending_queue.push(QueueItem::new("one"));
        store.commit(&mut session).unwrap();
        session.pending_queue[0].text = "rewrite".into();
        assert!(store.commit(&mut session).is_err());
        session.pending_queue[0].revision = 2;
        store.commit(&mut session).unwrap();
        assert_eq!(
            store.load(&session.id).unwrap().pending_queue[0].text,
            "rewrite"
        );
    }

    #[test]
    fn queued_attachments_round_trip_survive_edit_and_leave_with_their_item() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        let image = |name: &str, width| plakous_host_protocol::DraftImage {
            name: name.to_owned(),
            media_type: "image/png".to_owned(),
            bytes: plakous_hosted::attachments::png(width, 3, 100),
        };
        let mut with_images = QueueItem::new("look at these");
        with_images.images = vec![image("a.png", 2), image("b.png", 5)].into();
        session.pending_queue.push(with_images.clone());
        session.pending_queue.push(QueueItem::new("text only"));
        store.commit(&mut session).unwrap();
        let loaded = store.load(&session.id).unwrap();
        assert_eq!(loaded.pending_queue, session.pending_queue);
        assert_eq!(loaded.pending_queue[0].images.len(), 2);
        assert_eq!(loaded.pending_queue[0].images[1].name, "b.png");
        assert!(loaded.pending_queue[1].images.is_empty());

        // An edit replaces the text; the images stay with the item.
        session.pending_queue[0].text = "look again".into();
        session.pending_queue[0].revision = 2;
        store.commit(&mut session).unwrap();
        let edited = store.load(&session.id).unwrap();
        assert_eq!(edited.pending_queue[0].text, "look again");
        assert_eq!(edited.pending_queue[0].images, with_images.images);

        let attachments = |store: &ProductStore| -> i64 {
            store
                .connection
                .query_row("SELECT COUNT(*) FROM queue_attachments", [], |row| {
                    row.get(0)
                })
                .unwrap()
        };
        assert_eq!(attachments(&store), 2);
        session.pending_queue.remove(0);
        store.commit(&mut session).unwrap();
        assert_eq!(attachments(&store), 0);
        assert_eq!(store.load(&session.id).unwrap().pending_queue.len(), 1);
    }

    #[test]
    fn snapshot_load_rejects_submission_registry_detail_contradiction() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        let item = QueueItem::new("bound prompt");
        session.pending_queue.push(item.clone());
        let request = submission(&item, "bound-command");
        store.commit_submission(&mut session, &request).unwrap();
        store
            .connection
            .execute(
                "UPDATE identity_bindings SET binding_digest='contradiction' WHERE identity=?1",
                [request.binding_identity()],
            )
            .unwrap();
        assert!(store.load(&session.id).is_err());
        assert!(store.load_intents(&session.id).is_err());
        assert!(store.commit_submission(&mut session, &request).is_err());
    }

    #[test]
    fn every_intent_commit_restores_full_durability() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        let synchronous = |store: &ProductStore| -> i64 {
            store
                .connection
                .query_row("PRAGMA synchronous", [], |row| row.get(0))
                .unwrap()
        };
        let mut session = SessionRecord::new(root.path().to_path_buf());
        store.commit(&mut session).unwrap();
        assert_eq!(synchronous(&store), 2, "authority commits are FULL");
        project_rows(&mut store, &session.id, &[TranscriptRow::user("go")]);
        assert_eq!(synchronous(&store), 1, "projection commits are NORMAL");
        session.pending_queue.push(QueueItem::new("next"));
        store.commit(&mut session).unwrap();
        assert_eq!(synchronous(&store), 2);
        project_rows(&mut store, &session.id, &[TranscriptRow::assistant("ok")]);
        let item = session.pending_queue[0].clone();
        let request = submission(&item, "cmd-sync");
        store.commit_submission(&mut session, &request).unwrap();
        assert_eq!(synchronous(&store), 2, "a submission commit is FULL");
    }

    fn locator(branch: u8, checkpoint: u8) -> SessionLocator {
        format!(
            "00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-{branch:012}/00000000-0000-0000-0000-{checkpoint:012}"
        ).parse().unwrap()
    }

    fn other_runtime_locator(checkpoint: u8) -> SessionLocator {
        format!(
            "00000000-0000-0000-0000-000000000009/00000000-0000-0000-0000-000000000008/00000000-0000-0000-0000-{checkpoint:012}"
        ).parse().unwrap()
    }

    #[test]
    fn construction_binding_is_write_once() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("home"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut session = SessionRecord::new(root.path().to_owned());
        store.commit(&mut session).unwrap();
        session.bind_plakous_locator(locator(2, 3)).unwrap();
        store.commit(&mut session).unwrap();
        store.commit(&mut session).unwrap();
        session.bind_plakous_locator(locator(2, 4)).unwrap();
        store.commit(&mut session).unwrap();
        assert_eq!(
            store.load(&session.id).unwrap().plakous_locator,
            Some(locator(2, 3))
        );
    }

    #[test]
    fn view_head_wins_without_rewriting_construction_binding() {
        use plakous::sessions::feed::FeedLocator;

        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("home"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut session = SessionRecord::new(root.path().to_owned());
        session.bind_plakous_locator(locator(2, 3)).unwrap();
        store.commit(&mut session).unwrap();
        let empty = SessionViews::empty();
        let mut projected = empty.clone();
        let head = locator(2, 4);
        projected.state.runtime_session_id = Some(head.session_id());
        projected.state.head = Some(FeedLocator {
            session_id: head.session_id(),
            branch_id: head.branch_id(),
            checkpoint_id: head.checkpoint_id(),
        });
        store
            .commit_projection_views(
                &session.id,
                &ViewWrites::between(&empty, &projected),
                &projected,
            )
            .unwrap();
        let mut loaded = store.load(&session.id).unwrap();
        assert_eq!(loaded.plakous_locator, Some(head));
        assert_eq!(
            store.session_locators().unwrap(),
            vec![(session.id.clone(), Some(head))]
        );
        assert_eq!(
            store.list_for_workspace(&session.workspace_root).unwrap()[0].plakous_locator,
            Some(head)
        );
        loaded.title = Some("metadata at the projected head".to_owned());
        store.commit(&mut loaded).unwrap();
        store.delete_views(&session.id).unwrap();
        assert_eq!(
            store.load(&session.id).unwrap().plakous_locator,
            Some(locator(2, 3))
        );
    }

    #[test]
    fn legacy_workflow_rows_are_inert_for_record_and_request_operations() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        let source = locator(2, 3);
        session.bind_plakous_locator(source).unwrap();
        store.commit(&mut session).unwrap();
        store.connection.execute(
            "INSERT INTO workflows(session_id,operation_id,source_locator,kind,state) VALUES(?1,'obsolete','not-a-locator','compact','recovery_required')",
            [&session.id],
        ).unwrap();
        for action in ["INSERT", "UPDATE", "DELETE"] {
            store.connection.execute_batch(&format!(
                "CREATE TEMP TRIGGER refuse_workflow_{action} BEFORE {action} ON workflows BEGIN SELECT RAISE(ABORT,'legacy workflow write'); END;"
            )).unwrap();
        }
        let mut loaded = store.load(&session.id).unwrap();
        loaded.title = Some("still usable".into());
        store.commit(&mut loaded).unwrap();
        let request = plakous_hosted::sessions::projector::WorkflowRequest {
            operation_id: "plak:compact:request".into(),
            source_locator: source,
            kind: SessionWorkflowKind::Compact(ProductCompactReason::Scheduled),
        };
        store
            .submit_workflow_request(&mut loaded, &request)
            .unwrap();
        drop(store);
        let mut reopened = ProductStore::open(&paths).unwrap();
        assert_eq!(
            reopened.load(&session.id).unwrap().title.as_deref(),
            Some("still usable")
        );
        assert_eq!(
            reopened
                .load_intents(&session.id)
                .unwrap()
                .workflow_requests,
            vec![request]
        );
        assert_eq!(
            reopened
                .connection
                .query_row("SELECT count(*) FROM workflows", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn deletion_dependents_are_continuations_and_runtime_sharers_and_deletion_is_exact() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut source = SessionRecord::new(root.path().to_path_buf());
        source.bind_plakous_locator(locator(2, 3)).unwrap();
        store.commit(&mut source).unwrap();
        let runtime = locator(2, 3).session_id();
        assert_eq!(
            store.session_dependents(&source.id, runtime).unwrap(),
            [] as [String; 0]
        );

        // A fork's head is another branch of the same runtime session.
        let mut fork = SessionRecord::new(root.path().to_path_buf());
        fork.bind_plakous_locator(locator(4, 5)).unwrap();
        store.commit(&mut fork).unwrap();
        // A rebind or clear target continues from its source.
        let mut continuation = SessionRecord::new(root.path().to_path_buf());
        continuation.source_product_session_id = Some(source.id.clone());
        continuation.source_workflow_operation_id = Some("plak:clear:dependents-test".to_owned());
        continuation
            .bind_plakous_locator(other_runtime_locator(6))
            .unwrap();
        store.commit(&mut continuation).unwrap();
        let mut unrelated = SessionRecord::new(root.path().to_path_buf());
        unrelated
            .bind_plakous_locator(other_runtime_locator(7))
            .unwrap();
        // A carried-forward copy names its source but holds its own history.
        unrelated.link_carried_forward(&source.id, "previous-build");
        store.commit(&mut unrelated).unwrap();

        let mut dependents = store.session_dependents(&source.id, runtime).unwrap();
        dependents.sort();
        let mut expected = vec![fork.id.clone(), continuation.id.clone()];
        expected.sort();
        assert_eq!(dependents, expected);
        assert_eq!(
            store
                .session_dependents(&continuation.id, other_runtime_locator(6).session_id())
                .unwrap(),
            vec![unrelated.id.clone()]
        );

        // Deletion is exact to the checked revision and takes the owned rows.
        let stale = unrelated.store_revision;
        unrelated.record_notice(TranscriptRow::warning(NoticeScope::Tree, "moved"));
        store.commit(&mut unrelated).unwrap();
        assert!(store.delete_session(&unrelated.id, stale).is_err());
        store
            .delete_session(&unrelated.id, unrelated.store_revision)
            .unwrap();
        assert!(store.load(&unrelated.id).is_err());
        let orphaned: i64 = store
            .connection
            .query_row(
                "SELECT count(*) FROM operator_notices WHERE session_id = ?1",
                [&unrelated.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(orphaned, 0);
    }

    #[test]
    fn clear_publication_atomically_binds_distinct_workflow_and_create_identities() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut source = SessionRecord::new(root.path().to_path_buf());
        source.bind_plakous_locator(locator(2, 3)).unwrap();
        store.commit(&mut source).unwrap();
        let operation = "plak:clear:atomic-test";
        let request = plakous_hosted::sessions::projector::WorkflowRequest {
            operation_id: operation.to_owned(),
            source_locator: locator(2, 3),
            kind: SessionWorkflowKind::Clear,
        };
        store
            .submit_workflow_request(&mut source, &request)
            .unwrap();
        let published = store.load(&request.successor_product_id()).unwrap();
        assert_eq!(published.create_operation_id, format!("{operation}:create"));
        assert!(published.transcript.is_empty());

        assert_eq!(
            store.load_intents(&source.id).unwrap().workflow_requests,
            vec![request]
        );
    }
}
