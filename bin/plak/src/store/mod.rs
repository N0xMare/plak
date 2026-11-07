//! Private Plak product intents and feed-derived views.
//!
//! This store never opens the Plakous runtime database. Stable product and
//! command identities bind each submission to its exact immutable content.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use plakous::sessions::SessionLocator;
use plakous_hosted::error::StoreRefusal;
use plakous_hosted::session_store::RecordStoreError;
use plakous_hosted::sessions::TranscriptRow;
use plakous_recursive_subagents_store::admission::{self, AdmissionError, BUSY_TIMEOUT, Schema};
use rusqlite::{Connection, OpenFlags, OptionalExtension as _, Transaction, params};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;

use crate::home::paths::PlakPaths;
use crate::product_config::ProductCompositionSelection;
#[cfg(test)]
use crate::product_config::ProductTargetSelection;
use sessions::{
    ProductCompactReason, QueueDrainState, QueueItem, SessionNode, SessionRecord,
    SessionWorkflowKind, StoreBaseline, digest_parts,
};

pub(crate) const SCHEMA_VERSION: i64 = 37;
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
        Some(rusqlite::ErrorCode::DiskFull) => {
            RecordStoreError::refused(StoreRefusal::StorageFull, "product storage is full")
        }
        _ => "could not start a product store write".into(),
    }
}

fn write_error<E: std::error::Error + 'static>(
    error: E,
    detail: impl Into<String>,
) -> RecordStoreError {
    let source = &error as &dyn std::error::Error;
    if let Some(error) = source.downcast_ref::<RecordStoreError>() {
        return error.clone();
    }
    if source
        .downcast_ref::<rusqlite::Error>()
        .is_some_and(|error| error.sqlite_error_code() == Some(rusqlite::ErrorCode::DiskFull))
        || source
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| {
                error.raw_os_error().is_some_and(|code| {
                    code == rustix::io::Errno::NOSPC.raw_os_error()
                        || code == rustix::io::Errno::DQUOT.raw_os_error()
                })
            })
    {
        return RecordStoreError::refused(StoreRefusal::StorageFull, "product storage is full");
    }
    detail.into().into()
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
    /// The submissions this handle verified against their identity bindings.
    verified_submissions: views::VerifiedSubmissions,
    #[cfg(test)]
    capacity_fault: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
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
    fn recover_storage(&mut self) -> Result<(), RecordStoreError> {
        #[cfg(test)]
        self.capacity_fault()?;
        self.durability(true)?;
        // Dirty a real page and restore its value atomically. An optimized
        // no-op UPDATE would not establish that writes can commit.
        let tx = self
            .connection
            .transaction()
            .map_err(|error| write_error(error, "could not probe product storage"))?;
        tx.execute_batch("UPDATE product_schema SET fingerprint=fingerprint || 'x' WHERE singleton=1;
            UPDATE product_schema SET fingerprint=substr(fingerprint,1,length(fingerprint)-1) WHERE singleton=1;")
            .map_err(|error| write_error(error, "could not probe product storage"))?;
        tx.commit()
            .map_err(|error| write_error(error, "could not probe product storage"))?;
        Ok(())
    }

    /// Inspects one existing session record under a read-only SQLite snapshot.
    /// This does not create the home, open a writable store, or change product
    /// schema or session authority.
    pub fn inspect_session_read_only(paths: &PlakPaths, id: &str) -> Result<SessionRecord, String> {
        sessions::canonical_product_session_id(id)?;
        if matches!(std::fs::symlink_metadata(paths.product_database()), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        {
            return Err(crate::home::state::incompatible_session_refusal(paths, id)
                .unwrap_or_else(|| "session not found".to_owned()));
        }
        Self::read_only_snapshot(paths, |snapshot| {
            load_record(snapshot, &mut views::VerifiedSubmissions::default(), id)
        })
        .map_err(|error| {
            if error == "session not found" {
                crate::home::state::incompatible_session_refusal(paths, id).unwrap_or(error)
            } else {
                error
            }
        })
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
        SCHEMA.validate(&snapshot)?;
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
        let connection = admission::open_writer(&database, SCHEMA.noun)
            .map_err(ProductStoreOpenError::unavailable)?;
        #[cfg(feature = "test-hooks")]
        plakous_offline_crash_harness::commit_trace::install(&connection, "product")
            .map_err(|_| ProductStoreOpenError::unavailable("could not install commit trace"))?;
        let mut store = Self {
            connection,
            #[cfg(unix)]
            file_identity: (0, 0),
            baselines: HashMap::new(),
            synchronous_full: true,
            verified_submissions: views::VerifiedSubmissions::default(),
            #[cfg(test)]
            capacity_fault: None,
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

    fn admit_schema(&self) -> Result<(), ProductStoreOpenError> {
        #[cfg(feature = "test-hooks")]
        let _settle = SettleCommitTrace;
        SCHEMA.admit(&self.connection).map_err(|error| match error {
            AdmissionError::Unavailable(message) => ProductStoreOpenError::unavailable(message),
            AdmissionError::Unsupported(message) => ProductStoreOpenError::unsupported(message),
        })
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

    pub fn commit(&mut self, session: &mut SessionRecord) -> Result<(), RecordStoreError> {
        self.commit_with_submission(session, None, None, None)
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
    ) -> Result<(), RecordStoreError> {
        self.commit_with_submission(session, None, Some(command_id), pause_cause)
    }

    fn commit_with_submission(
        &mut self,
        session: &mut SessionRecord,
        submission: Option<&plakous_hosted::sessions::projector::Submission>,
        discarded: Option<&str>,
        pause_cause: Option<plakous_hosted::sessions::projector::HostPauseCause>,
    ) -> Result<(), RecordStoreError> {
        #[cfg(test)]
        self.capacity_fault()?;
        #[cfg(feature = "test-hooks")]
        let _settle = SettleCommitTrace;
        let expected = session.store_revision;
        self.durability(true)?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(write_refusal)?;
        if submission.is_some() && session.archived {
            return Err("session_archived: unarchive this session before admitting work".into());
        }
        if submission.is_some() && expected > 0 {
            lifecycle::admit_work(&transaction, &session.id)?;
        }
        let next = commit_in_transaction(
            &transaction,
            session,
            expected,
            self.baselines.get(&session.id),
        )?;
        if let Some(submission) = submission {
            views::insert_submission(
                &transaction,
                &mut self.verified_submissions,
                &session.id,
                submission,
            )?;
        }
        if let Some(command_id) = discarded {
            self.verified_submissions.forget(&session.id, command_id);
            transaction
                .execute(
                    "DELETE FROM submissions WHERE session_id=?1 AND command_id=?2",
                    params![session.id, command_id],
                )
                .map_err(|error| {
                    write_error(error, "could not discard command submission".to_owned())
                })?;
        }
        if let Some(cause) = pause_cause {
            views::insert_pause_cause(&transaction, &session.id, &cause)?;
        }
        let queue_commands =
            views::queue_commands_of(&transaction, &mut self.verified_submissions, &session.id)?;
        let operator_fields = transaction
            .query_row(
                "SELECT title_override,archived FROM sessions WHERE id=?1",
                [&session.id],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, bool>(1)?)),
            )
            .map_err(|error| write_error(error, "could not read operator metadata"))?;
        transaction.commit().map_err(|error| {
            write_error(error, "could not durably commit product session".to_owned())
        })?;
        session.store_revision = next;
        session.title_override = operator_fields.0;
        session.archived = operator_fields.1;
        session.transcript.mark_committed();
        self.remember(session);
        session.queue_commands = queue_commands;
        Ok(())
    }

    #[cfg(test)]
    fn capacity_fault(&self) -> Result<(), RecordStoreError> {
        if self
            .capacity_fault
            .as_ref()
            .is_some_and(|full| full.load(std::sync::atomic::Ordering::Acquire))
        {
            return Err(write_error(
                std::io::Error::from_raw_os_error(rustix::io::Errno::NOSPC.raw_os_error()),
                "injected write refusal",
            ));
        }
        Ok(())
    }

    pub fn load_queue_head(
        &mut self,
        id: &str,
    ) -> Result<plakous_hosted::session_store::SessionQueueHead, String> {
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(|_| "could not start product store read".to_owned())?;
        let locator: Option<String> = transaction
            .query_row("SELECT locator FROM sessions WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .map_err(|error| {
                if matches!(error, rusqlite::Error::QueryReturnedNoRows) {
                    "session not found".to_owned()
                } else {
                    "product session locator is invalid".to_owned()
                }
            })?;
        let recorded_locator = locator
            .map(|locator| locator.parse())
            .transpose()
            .map_err(|_| "product session locator is invalid".to_owned())?;
        let pending_queue = load_queue(&transaction, id)?;
        transaction
            .commit()
            .map_err(|_| "could not finish product store read".to_owned())?;
        Ok(plakous_hosted::session_store::SessionQueueHead {
            pending_queue,
            recorded_locator,
        })
    }

    pub fn load(&mut self, id: &str) -> Result<SessionRecord, String> {
        #[cfg(feature = "test-hooks")]
        let _settle = SettleCommitTrace;
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(|_| "could not start product store read".to_owned())?;
        let session = load_record(&transaction, &mut self.verified_submissions, id)?;
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

fn load_record(
    connection: &Connection,
    verified: &mut views::VerifiedSubmissions,
    id: &str,
) -> Result<SessionRecord, String> {
    let mut session = connection
            .query_row(
                "SELECT revision, create_operation_id, workspace_root, created_at, updated_at, account, model, effort, title, source_session_id, source_operation_id, locator, composition, target_selection, frozen_tools, carry_intent, node_tree_id, node_agent_id, root_profile, runner_display, title_override, archived FROM sessions WHERE id = ?1",
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
                        carry_intent: row.get::<_, Option<String>>(15)?.map(|value| plakous_hosted::sessions::CarryIntent::decode(&value).map_err(|_| rusqlite::Error::InvalidQuery)).transpose()?,
                        node: decode_node(row, 16)?,
                        root_profile: decode_root_profile(row, 18)?,
                        runner: decode_runner_display(row, 19)?,
                        title_override: row.get(20)?,
                        archived: row.get(21)?,
                    })
                },
            )
            .map_err(|error| if matches!(error, rusqlite::Error::QueryReturnedNoRows) { "session not found".to_owned() } else { "product session row is invalid".to_owned() })?;
    session.pending_queue = load_queue(connection, id)?;
    let (transcript, views, queue_commands) = views::transcript_of(connection, verified, id)?;
    session.transcript = transcript;
    let intents = views::intents_of(connection, verified, id)?;
    let empty = plakous_hosted::sessions::projector::SessionViews::empty();
    let joined = views.as_ref().unwrap_or(&empty);
    session.child_wake_suppressed = joined.wake_suppressed(&intents);
    if joined.pause_cause(&intents).is_some() {
        session.queue_drain_state = QueueDrainState::RestoredPaused;
    }
    if let Some(views) = &views {
        session.plakous_locator = views.projected_locator(session.plakous_locator);
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
        #[cfg(test)]
        lifecycle::crash_cut("delete_before_commit");
        transaction
            .commit()
            .map_err(|_| "could not durably delete the product session".to_owned())?;
        #[cfg(test)]
        lifecycle::crash_cut("delete_after_commit");
        self.baselines.remove(id);
        self.verified_submissions.forget_session(id);
        Ok(())
    }

    pub fn list_for_workspace(&mut self, workspace: &Path) -> Result<Vec<SessionRecord>, String> {
        self.list_for_workspace_with_query_count(workspace)
            .map(|(sessions, _)| sessions)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Session listing batches summaries, workflow state, and retained branches while counting the exact queries used for one workspace view."
    )]
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
            "SELECT id,create_operation_id,workspace_root,created_at,updated_at,account,model,effort,title,source_session_id,source_operation_id,CASE WHEN v.session_id IS NULL THEN sessions.locator ELSE v.head_locator END,composition,target_selection,COALESCE(v.root_turn_count,0),COALESCE(v.tool_call_count,0),COALESCE(v.compaction_count,0),sessions.carry_intent,sessions.node_tree_id,sessions.node_agent_id,sessions.root_profile,sessions.runner_display,sessions.title_override,sessions.archived FROM sessions LEFT JOIN session_views v ON v.session_id=sessions.id WHERE workspace_identity = ?1 ORDER BY updated_at DESC, id ASC LIMIT ?2"
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
                    carry_intent: row
                        .get::<_, Option<String>>(17)?
                        .map(|value| {
                            plakous_hosted::sessions::CarryIntent::decode(&value)
                                .map_err(|_| rusqlite::Error::InvalidQuery)
                        })
                        .transpose()?,
                    node: decode_node(row, 18)?,
                    root_profile: decode_root_profile(row, 20)?,
                    runner: decode_runner_display(row, 21)?,
                    title_override: row.get(22)?,
                    archived: row.get(23)?,
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

fn insert_session(tx: &Transaction<'_>, s: &SessionRecord) -> Result<(), RecordStoreError> {
    let workspace = s.workspace_root.to_string_lossy().into_owned();
    let workspace_identity = sessions::workspace_identity(&s.workspace_root)
        .to_string_lossy()
        .into_owned();
    let locator = s.plakous_locator.map(|value| value.to_string());
    let changed = tx.execute(
        "INSERT INTO sessions(id, revision, create_operation_id, workspace_root, workspace_identity, created_at, updated_at, account, model, effort, title, source_session_id, source_operation_id, locator, composition, target_selection, frozen_tools, carry_intent, node_tree_id, node_agent_id, root_profile, runner_display, title_override, archived) VALUES (?1,1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23)",
        params![s.id, s.create_operation_id, workspace, workspace_identity, to_i64(s.created_at_unix)?, to_i64(s.updated_at_unix)?, s.account, s.model, s.effort, s.title, s.source_product_session_id, s.source_workflow_operation_id, locator, s.composition.as_str(), serde_json::to_string(&s.target_selection).map_err(|_|"target selection is not serializable")?, encoded_frozen_tools(s)?, encoded_carry_intent(s)?, s.node.tree_id, s.node.agent_id, encoded_root_profile(s)?, encoded_runner_display(s)?, s.title_override, s.archived],
    ).map_err(|error| write_error(error, "product session identity already exists or is invalid".to_owned()))?;
    if changed != 1 {
        return Err("product session insert was not exact".to_owned().into());
    }
    Ok(())
}

fn commit_in_transaction(
    tx: &Transaction<'_>,
    session: &SessionRecord,
    expected: u64,
    baseline: Option<&StoreBaseline>,
) -> Result<u64, RecordStoreError> {
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
            return Err("product session changed concurrently".to_owned().into());
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
) -> Result<usize, RecordStoreError> {
    // A loaded record carries the view head, not a replacement genesis binding.
    tx.execute(
        "UPDATE sessions SET revision=?2, workspace_root=?4, workspace_identity=?5, updated_at=?7, account=?8, model=?9, effort=?10, title=?11, source_session_id=?12, source_operation_id=?13, locator=COALESCE(locator,?14), composition=?15, target_selection=?17, frozen_tools=COALESCE(frozen_tools,?18), carry_intent=?19, root_profile=COALESCE(root_profile,?22) WHERE id=?1 AND revision=?3 AND create_operation_id=?16 AND created_at=?6 AND node_tree_id=?20 AND node_agent_id=?21 AND (root_profile IS NULL OR root_profile IS ?22) AND runner_display IS ?23",
        params![s.id, to_i64(next)?, to_i64(expected)?, s.workspace_root.to_string_lossy(), sessions::workspace_identity(&s.workspace_root).to_string_lossy(), to_i64(s.created_at_unix)?, to_i64(s.updated_at_unix)?, s.account, s.model, s.effort, s.title, s.source_product_session_id, s.source_workflow_operation_id, s.plakous_locator.map(|v| v.to_string()), s.composition.as_str(), s.create_operation_id, serde_json::to_string(&s.target_selection).map_err(|_|"target selection is not serializable")?, encoded_frozen_tools(s)?, encoded_carry_intent(s)?, s.node.tree_id, s.node.agent_id, encoded_root_profile(s)?, encoded_runner_display(s)?],
    ).map_err(|error| write_error(error, "could not update product session".to_owned()))
}

/// The root profile, written once at genesis (a reservation may gain it
/// before its runtime create), and the runner display copy, written with the
/// record; a save that moved either is refused like a moved node.
fn encoded_root_profile(s: &SessionRecord) -> Result<Option<String>, String> {
    s.root_profile
        .as_ref()
        .map(plakous_hosted::sessions::RootProfile::encode)
        .transpose()
}

fn encoded_runner_display(s: &SessionRecord) -> Result<Option<String>, String> {
    s.runner
        .as_ref()
        .map(plakous_hosted::sessions::RunnerDisplay::encode)
        .transpose()
}

/// The record's node, written once at creation (spec 02 §2.6).
fn decode_node(row: &rusqlite::Row<'_>, first: usize) -> rusqlite::Result<SessionNode> {
    SessionNode::parse(
        &row.get::<_, String>(first)?,
        &row.get::<_, String>(first + 1)?,
    )
    .map_err(|_| rusqlite::Error::InvalidQuery)
}

fn decode_root_profile(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> rusqlite::Result<Option<plakous_hosted::sessions::RootProfile>> {
    row.get::<_, Option<String>>(index)?
        .map(|value| {
            plakous_hosted::sessions::RootProfile::decode(&value)
                .map_err(|_| rusqlite::Error::InvalidQuery)
        })
        .transpose()
}

fn decode_runner_display(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> rusqlite::Result<Option<plakous_hosted::sessions::RunnerDisplay>> {
    row.get::<_, Option<String>>(index)?
        .map(|value| {
            plakous_hosted::sessions::RunnerDisplay::decode(&value)
                .map_err(|_| rusqlite::Error::InvalidQuery)
        })
        .transpose()
}

/// A carry's write-ahead intent; the save that binds its locator clears it.
fn encoded_carry_intent(s: &SessionRecord) -> Result<Option<String>, String> {
    s.carry_intent
        .as_ref()
        .map(plakous_hosted::sessions::CarryIntent::encode)
        .transpose()
}

/// The root's frozen tools are written once, with the first runtime
/// creation; a later save never replaces them.
fn encoded_frozen_tools(s: &SessionRecord) -> Result<Option<String>, String> {
    s.frozen_tools
        .as_ref()
        .map(plakous_hosted::frozen_tools::FrozenRootTools::encode)
        .transpose()
}

fn sync_queue(tx: &Transaction<'_>, s: &SessionRecord) -> Result<(), RecordStoreError> {
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
            return Err("queue tombstone contradicts durable state"
                .to_owned()
                .into());
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
        .map_err(|error| write_error(error, "could not stage product queue reorder".to_owned()))?;
    }
    for (position, item) in s.pending_queue.iter().enumerate() {
        let changed = tx.execute("INSERT INTO queue_items(session_id,item_id,revision,position,text,submitted_at) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(item_id) DO UPDATE SET revision=excluded.revision,position=excluded.position,text=excluded.text WHERE session_id=excluded.session_id AND ((queue_items.revision=excluded.revision AND queue_items.text=excluded.text) OR queue_items.revision+1=excluded.revision)",
            params![s.id,item.id,to_i64(item.revision)?,i64::try_from(position).map_err(|error| write_error(error, "queue position is invalid"))?,item.text,to_i64(item.submitted_at_ms)?])
            .map_err(|error| write_error(error, "could not commit product queue".to_owned()))?;
        if changed != 1 {
            return Err("queue identity contradicts durable product state"
                .to_owned()
                .into());
        }
        // Attachments are immutable per item: written once with the item,
        // removed with it by the foreign-key cascade.
        if !existing.iter().any(|(id, _, _)| id == &item.id) {
            for (ordinal, image) in item.images.iter().enumerate() {
                tx.execute(
                    "INSERT INTO queue_attachments(item_id,ordinal,name,media_type,bytes) VALUES(?1,?2,?3,?4,?5)",
                    params![
                        item.id,
                        i64::try_from(ordinal).map_err(|error| write_error(error, "attachment ordinal is invalid"))?,
                        image.name,
                        image.media_type,
                        image.bytes
                    ],
                )
                .map_err(|error| write_error(error, "could not commit product queue attachments".to_owned()))?;
            }
        }
    }
    Ok(())
}

fn sync_prompt_history(tx: &Transaction<'_>, s: &SessionRecord) -> Result<(), RecordStoreError> {
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
            .map_err(|error| {
                write_error(error, "could not prune product prompt history".to_owned())
            })?
            != 1
        {
            return Err("product prompt history changed concurrently"
                .to_owned()
                .into());
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
            .map_err(|error| {
                write_error(error, "could not commit product prompt history".to_owned())
            })?;
        if changed != 1 {
            return Err("could not commit exact product prompt history"
                .to_owned()
                .into());
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
) -> Result<(), RecordStoreError> {
    let key = if table == "queue_items" {
        "item_id"
    } else {
        "operation_id"
    };
    let mut statement = tx
        .prepare(&format!("SELECT {key} FROM {table} WHERE session_id=?1"))
        .map_err(|error| write_error(error, "could not inspect product rows".to_owned()))?;
    let existing = statement
        .query_map([session], |row| row.get::<_, String>(0))
        .map_err(|error| write_error(error, "could not inspect product rows".to_owned()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| write_error(error, "product row identity is invalid".to_owned()))?;
    let keep = keep.collect::<HashSet<_>>();
    for id in existing {
        if !keep.contains(id.as_str()) {
            let changed = tx
                .execute(
                    &format!("DELETE FROM {table} WHERE session_id=?1 AND {key}=?2"),
                    params![session, id],
                )
                .map_err(|error| write_error(error, "could not remove product row".to_owned()))?;
            if changed != 1 {
                return Err("product row changed during exact removal".to_owned().into());
            }
        }
    }
    Ok(())
}

/// Claims or terminalizes one permanent identity. No production path gives a
/// `command` binding a terminal disposition; one that does must also forget
/// the submission in [`views::VerifiedSubmissions`], which trusts a verified
/// command binding for the connection's lifetime.
fn bind_identity(
    tx: &Transaction<'_>,
    kind: &str,
    identity: &str,
    owner: &str,
    binding: &str,
    terminal_disposition: Option<&str>,
    terminal_outcome: Option<&str>,
) -> Result<(), RecordStoreError> {
    let changed = tx.execute(
        "INSERT INTO identity_bindings(identity,kind,owner_session_id,binding_digest,terminal_disposition,terminal_outcome) VALUES(?1,?2,?3,?4,?5,?6)
         ON CONFLICT(identity) DO UPDATE SET terminal_disposition=excluded.terminal_disposition,terminal_outcome=excluded.terminal_outcome
         WHERE kind=excluded.kind AND owner_session_id=excluded.owner_session_id
           AND binding_digest=excluded.binding_digest
           AND (terminal_disposition IS NULL OR terminal_disposition=excluded.terminal_disposition)
           AND (terminal_outcome IS NULL OR terminal_outcome IS excluded.terminal_outcome)
           AND NOT(terminal_disposition IS NOT NULL AND excluded.terminal_disposition IS NULL)",
        params![identity, kind, owner, binding, terminal_disposition,terminal_outcome],
    ).map_err(|error| write_error(error, "stable product identity contradicts durable state".to_owned()))?;
    if changed != 1 {
        return Err("stable product identity contradicts durable state"
            .to_owned()
            .into());
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
                    // The queue keeps the image as sent, not how it was attached.
                    source_dimensions: None,
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
        "new" => Ok(SessionWorkflowKind::New),
        "fork" => Ok(SessionWorkflowKind::Fork),
        "rebind" => Ok(SessionWorkflowKind::Rebind(Box::new(
            serde_json::from_str(&selection.ok_or(rusqlite::Error::InvalidQuery)?)
                .map_err(|_| rusqlite::Error::InvalidQuery)?,
        ))),
        "switch" => Ok(SessionWorkflowKind::Switch(Box::new(
            sessions::SwitchSelection::decode(&selection.ok_or(rusqlite::Error::InvalidQuery)?)
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
        "CREATE TABLE sessions(id TEXT PRIMARY KEY,revision INTEGER NOT NULL CHECK(revision>0),create_operation_id TEXT NOT NULL UNIQUE,workspace_root TEXT NOT NULL,workspace_identity TEXT NOT NULL,created_at INTEGER NOT NULL CHECK(created_at>=0),updated_at INTEGER NOT NULL CHECK(updated_at>=0),account TEXT,model TEXT,effort TEXT,title TEXT,source_session_id TEXT,source_operation_id TEXT,locator TEXT, composition TEXT NOT NULL DEFAULT 'plakous-default' CHECK(composition IN('direct','plakous-default','native-ptc','js-ptc','py-ptc','py-sdk')), target_selection TEXT NOT NULL DEFAULT '\"native\"', frozen_tools TEXT CHECK(frozen_tools IS NULL OR json_valid(frozen_tools)), carry_intent TEXT CHECK(carry_intent IS NULL OR (json_valid(carry_intent) AND locator IS NULL)), node_tree_id TEXT NOT NULL CHECK(length(node_tree_id) BETWEEN 1 AND 128), node_agent_id TEXT NOT NULL CHECK(length(node_agent_id) BETWEEN 1 AND 128), root_profile TEXT CHECK(root_profile IS NULL OR json_valid(root_profile)), runner_display TEXT CHECK(runner_display IS NULL OR json_valid(runner_display)), title_override TEXT, archived INTEGER NOT NULL CHECK(archived IN (0,1)))",
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
        "CREATE TABLE workflow_requests(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,operation_id TEXT PRIMARY KEY,source_locator TEXT NOT NULL,kind TEXT NOT NULL CHECK(kind IN('compact','new','fork','rebind','switch')),compact_reason TEXT,selection TEXT,anchor INTEGER NOT NULL CHECK(anchor>=0))",
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
        "CREATE TABLE host_intents(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,kind TEXT NOT NULL CHECK(kind IN('wake_suppression','pause_cause','interrupt_cause','continuation_decision')),anchor INTEGER NOT NULL CHECK(anchor>=0),ordinal INTEGER NOT NULL CHECK(ordinal>=0),value TEXT NOT NULL CHECK(json_valid(value)),PRIMARY KEY(session_id,kind,anchor,ordinal))",
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

/// The product file's admission descriptors.
const SCHEMA: Schema = Schema {
    noun: "product",
    stem: "product",
    archive: "product.sqlite3 and runtime.sqlite3 together",
    version: SCHEMA_VERSION,
    objects: SCHEMA_OBJECTS,
};

/// Operator acknowledgements of one attention occurrence.
pub mod acknowledgements;
#[cfg(feature = "test-support")]
pub(crate) mod benchmark;
/// What a host's session directory reads.
pub mod directory;
/// Who holds a session another process has open.
pub mod holders;
mod lifecycle;
#[cfg(test)]
pub(crate) use lifecycle::crash_cut as lifecycle_crash_cut;
/// The product's SQLite implementation of the hosted session-record port.
pub mod session_records;
/// The product session record's durable half: the free functions that open
/// `product.sqlite3` and the one-entry store cache behind them.
pub mod sessions;
/// The product's SQLite implementation of the tree store.
pub mod tree;
/// Schema 31 intents and the projector's views.
pub mod views;

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
mod tests;
