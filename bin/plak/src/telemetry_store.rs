//! Disposable local analytics store for typed plakous telemetry observations.

use std::path::{Path, PathBuf};
use std::time::Duration;

use plakous::agent::{
    CompactionEventOutcome, CompactionEventReason, CompactionRefusalReason,
    CredentialRefreshFailureClass, CredentialRefreshPhase, DurabilityAttemptKind,
    DurabilityAttemptOutcome, HttpStatusClass, TelemetryObservation, TelemetryObservationKind,
    TelemetryTerminalStatus,
};
use plakous::model::{Timestamp, Usage};
use rusqlite::{Connection, OpenFlags, params};

use crate::home::paths::PlakPaths;
use plakous_hosted::product_observation::{ChildKind, ProductObservation};

pub(crate) const TELEMETRY_SCHEMA_VERSION: i64 = 19;

/// Default disposable telemetry retention.
pub const DEFAULT_TELEMETRY_MAX_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_TELEMETRY_MAX_AGE_DAYS: u64 = 90;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Truncate checkpoints must not stall the writer behind a reader.
const CHECKPOINT_BUSY_TIMEOUT: Duration = Duration::from_millis(25);
const BATCH_DRAIN: usize = 255;
pub(crate) const WAL_CHECKPOINT_EVERY_WRITES: u32 = 32;
const MAX_BYTE_PRUNE_PASSES: u32 = 64;

const SCHEMA_DDL: &str = r"
CREATE TABLE job_events (
  id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
  at INTEGER NOT NULL,
  session_id TEXT NOT NULL,
  branch_id TEXT NOT NULL,
  run_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  turn_id TEXT,
  call_id TEXT,
  kind TEXT NOT NULL,
  tool TEXT,
  nested INTEGER,
  hint_source TEXT,
  hint_ms INTEGER,
  delivery_point TEXT,
  count INTEGER
);
CREATE INDEX job_events_session_id ON job_events(session_id);

CREATE TABLE model_retries (
  id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
  at INTEGER NOT NULL,
  session_id TEXT NOT NULL,
  branch_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  turn_id TEXT NOT NULL,
  next_attempt INTEGER NOT NULL,
  max_attempts INTEGER NOT NULL,
  cause_kind TEXT NOT NULL,
  transport_phase TEXT NOT NULL,
  assistant_text_bytes INTEGER NOT NULL,
  reasoning_bytes INTEGER NOT NULL,
  completed_items INTEGER NOT NULL,
  elapsed_ms INTEGER NOT NULL,
  replay_safety TEXT NOT NULL,
  declined_reason TEXT
);
CREATE INDEX model_retries_session_id ON model_retries(session_id);
CREATE VIEW v_replay_loss AS SELECT * FROM model_retries;

CREATE TABLE turns (
  session_id TEXT NOT NULL,
  branch_id TEXT NOT NULL,
  run_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  turn_id TEXT NOT NULL,
  started_at INTEGER,
  queue_ns INTEGER,
  origin TEXT,
  finished_at INTEGER,
  elapsed_ns INTEGER,
  status TEXT,
  model_wall_ns INTEGER,
  tool_wall_ns INTEGER,
  compaction_wall_ns INTEGER,
  model_attempts INTEGER,
  tool_calls INTEGER,
  compactions INTEGER,
  input_tokens INTEGER,
  output_tokens INTEGER,
  reasoning_tokens INTEGER,
  cached_input_tokens INTEGER,
  cache_write_tokens INTEGER,
  cost_micros INTEGER,
  cost_status TEXT NOT NULL DEFAULT 'unknown',
  PRIMARY KEY (turn_id)
);

CREATE TABLE model_attempts (
  attempt_id TEXT NOT NULL PRIMARY KEY,
  session_id TEXT NOT NULL,
  branch_id TEXT NOT NULL,
  run_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  turn_id TEXT NOT NULL,
  started_at INTEGER,
  first_event_at INTEGER,
  ttft_ns INTEGER,
  last_semantic_event_at INTEGER,
  keepalive_count INTEGER NOT NULL DEFAULT 0,
  max_semantic_gap_ms INTEGER NOT NULL DEFAULT 0,
  finished_at INTEGER,
  ttlb_ns INTEGER,
  status TEXT,
  input_tokens INTEGER,
  output_tokens INTEGER,
  reasoning_tokens INTEGER,
  cached_input_tokens INTEGER,
  cache_write_tokens INTEGER,
  served_endpoint TEXT,
  served_path_fingerprint TEXT,
  cost_micros INTEGER,
  cost_status TEXT NOT NULL DEFAULT 'unknown'
);

CREATE TABLE tool_calls (
  call_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  branch_id TEXT NOT NULL,
  run_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  turn_id TEXT NOT NULL,
  attempt_id TEXT,
  started_at INTEGER,
  finished_at INTEGER,
  elapsed_ns INTEGER,
  status TEXT,
  output_bytes INTEGER,
  tool_name TEXT,
  PRIMARY KEY (turn_id, call_id)
);

CREATE TABLE compactions (
  id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL,
  branch_id TEXT NOT NULL,
  run_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  turn_id TEXT,
  started_at INTEGER,
  finished_at INTEGER,
  elapsed_ns INTEGER,
  reason TEXT NOT NULL,
  outcome TEXT,
  refusal_reason TEXT,
  tokens_before INTEGER,
  tokens_after INTEGER,
  input_tokens INTEGER,
  output_tokens INTEGER,
  reasoning_tokens INTEGER,
  cached_input_tokens INTEGER,
  cache_write_tokens INTEGER,
  cost_micros INTEGER,
  cost_status TEXT NOT NULL DEFAULT 'unknown'
);

CREATE TABLE durability_attempts (
  id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL,
  branch_id TEXT NOT NULL,
  run_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  turn_id TEXT,
  at INTEGER NOT NULL,
  elapsed_ns INTEGER NOT NULL,
  kind TEXT NOT NULL,
  attempt TEXT,
  outcome TEXT,
  status TEXT
);

CREATE TABLE retention_runs (
  id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL,
  branch_id TEXT NOT NULL,
  run_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  turn_id TEXT,
  at INTEGER NOT NULL,
  elapsed_ns INTEGER NOT NULL,
  status TEXT NOT NULL,
  pruned_checkpoints INTEGER NOT NULL
);

CREATE TABLE product_events (
  id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
  at INTEGER NOT NULL,
  kind TEXT NOT NULL,
  session_id TEXT NOT NULL,
  tree_id TEXT,
  node_id TEXT,
  parent_node_id TEXT,
  agent_id TEXT,
  spawn_class TEXT,
  elapsed_ns INTEGER,
  status TEXT
);

CREATE TABLE resource_samples (
  id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
  at INTEGER NOT NULL,
  session_id TEXT NOT NULL,
  agent_id TEXT,
  phase TEXT NOT NULL,
  child_kind TEXT,
  rss_bytes INTEGER,
  peak_rss_bytes INTEGER,
  user_cpu_ns INTEGER NOT NULL,
  system_cpu_ns INTEGER NOT NULL,
  threads INTEGER,
  fds INTEGER
);

CREATE TABLE credential_refreshes (
  id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
  at INTEGER NOT NULL,
  provider TEXT NOT NULL,
  phase TEXT NOT NULL,
  failure_class TEXT,
  transport_phase TEXT,
  http_status INTEGER,
  elapsed_ns INTEGER,
  generation INTEGER NOT NULL,
  failure_reason TEXT
);

CREATE TABLE prompt_cache_diagnoses (
  id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
  at INTEGER NOT NULL,
  session_id TEXT NOT NULL,
  branch_id TEXT NOT NULL,
  run_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  turn_id TEXT,
  attempt_id TEXT,
  reason TEXT NOT NULL,
  missed_input_tokens INTEGER
);

CREATE INDEX turns_session_id ON turns(session_id);
CREATE INDEX turns_agent_id ON turns(agent_id);
CREATE INDEX model_attempts_session_id ON model_attempts(session_id);
CREATE INDEX model_attempts_agent_id ON model_attempts(agent_id);
CREATE INDEX model_attempts_turn_id ON model_attempts(turn_id);
CREATE INDEX tool_calls_session_id ON tool_calls(session_id);
CREATE INDEX tool_calls_agent_id ON tool_calls(agent_id);
CREATE INDEX compactions_session_id ON compactions(session_id);
CREATE INDEX compactions_turn_id ON compactions(turn_id);
CREATE INDEX durability_attempts_session_id ON durability_attempts(session_id);
CREATE INDEX durability_attempts_turn_id ON durability_attempts(turn_id);
CREATE INDEX retention_runs_session_id ON retention_runs(session_id);
CREATE INDEX product_events_session_id ON product_events(session_id);
CREATE INDEX resource_samples_session_id ON resource_samples(session_id);
CREATE INDEX credential_refreshes_provider_at ON credential_refreshes(provider, at);
CREATE INDEX prompt_cache_diagnoses_session_id ON prompt_cache_diagnoses(session_id);

CREATE VIEW v_turn_costs AS
SELECT
  t.turn_id,
  t.session_id,
  t.agent_id,
  t.elapsed_ns,
  t.model_wall_ns,
  t.tool_wall_ns,
  t.compaction_wall_ns,
  COALESCE(SUM(m.input_tokens), 0) AS input_tokens,
  COALESCE(SUM(m.output_tokens), 0) AS output_tokens,
  COALESCE(SUM(m.reasoning_tokens), 0) AS reasoning_tokens,
  COALESCE(SUM(m.cached_input_tokens), 0) AS cached_input_tokens,
  COALESCE(SUM(m.cache_write_tokens), 0) AS cache_write_tokens,
  AVG(m.ttft_ns) AS avg_ttft_ns,
  t.cost_micros,
  t.cost_status,
  CAST(t.cost_micros AS REAL) / 1000000.0 AS cost_usd,
  GROUP_CONCAT(DISTINCT m.served_endpoint) AS served_endpoint
FROM turns t
LEFT JOIN model_attempts m ON m.turn_id = t.turn_id
GROUP BY t.turn_id;

CREATE VIEW v_agent_tokens AS
SELECT
  agent_id,
  session_id,
  COALESCE(SUM(input_tokens), 0) AS input_tokens,
  COALESCE(SUM(output_tokens), 0) AS output_tokens,
  COALESCE(SUM(reasoning_tokens), 0) AS reasoning_tokens,
  COALESCE(SUM(cached_input_tokens), 0) AS cached_input_tokens,
  COALESCE(SUM(cache_write_tokens), 0) AS cache_write_tokens
FROM model_attempts
GROUP BY session_id, agent_id;

CREATE VIEW v_tool_success AS
SELECT
  turn_id,
  session_id,
  agent_id,
  SUM(CASE WHEN status = 'completed' THEN 1 ELSE 0 END) AS completed,
  SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END) AS failed,
  SUM(CASE WHEN status = 'cancelled' THEN 1 ELSE 0 END) AS cancelled,
  COUNT(*) AS total
FROM tool_calls
WHERE finished_at IS NOT NULL
GROUP BY turn_id;

CREATE VIEW v_child_resources AS
SELECT
  session_id,
  child_kind,
  SUM(user_cpu_ns) AS user_cpu_ns,
  SUM(system_cpu_ns) AS system_cpu_ns,
  MAX(peak_rss_bytes) AS peak_rss_bytes
FROM resource_samples
WHERE child_kind IS NOT NULL
GROUP BY session_id, child_kind;

CREATE VIEW v_credential_refreshes AS
SELECT
  id,
  provider,
  phase,
  failure_class,
  transport_phase,
  CASE
    WHEN http_status IS NULL THEN NULL
    WHEN http_status >= 500 THEN '5xx'
    WHEN http_status >= 400 THEN '4xx'
    WHEN http_status >= 300 THEN '3xx'
    WHEN http_status >= 200 THEN '2xx'
    ELSE '1xx'
  END AS http_status_class,
  http_status,
  elapsed_ns,
  generation,
  failure_reason,
  at
FROM credential_refreshes;
";

/// Result of `PRAGMA wal_checkpoint(TRUNCATE)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalCheckpointOutcome {
    /// The WAL was checkpointed and truncated.
    Completed,
    /// A reader still holds the log; the file was not truncated.
    Busy,
}

impl WalCheckpointOutcome {
    #[must_use]
    pub const fn is_busy(self) -> bool {
        matches!(self, Self::Busy)
    }

    const fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Busy, _) | (_, Self::Busy) => Self::Busy,
            _ => Self::Completed,
        }
    }
}

/// Result of applying retention, including whether the WAL could be truncated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TelemetryPruneOutcome {
    pub wal_checkpoint: WalCheckpointOutcome,
}

/// Retention caps applied on writer startup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TelemetryRetentionConfig {
    pub max_bytes: u64,
    pub max_age_days: u64,
}

impl TelemetryRetentionConfig {
    #[must_use]
    pub const fn defaults() -> Self {
        Self {
            max_bytes: DEFAULT_TELEMETRY_MAX_BYTES,
            max_age_days: DEFAULT_TELEMETRY_MAX_AGE_DAYS,
        }
    }
}

impl From<crate::product_config::TelemetryConfig> for TelemetryRetentionConfig {
    fn from(config: crate::product_config::TelemetryConfig) -> Self {
        Self {
            max_bytes: config.max_bytes,
            max_age_days: config.max_age_days,
        }
    }
}

/// One item enqueued for the telemetry writer thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TelemetryItem {
    Agent(TelemetryObservation),
    Product(ProductObservation),
}

pub(crate) fn emit_projected_turn_usages(
    paths: &PlakPaths,
    terminals: &[plakous_hosted::sessions::projector::ProjectedTurnUsage],
) {
    let observations: Vec<_> = terminals
        .iter()
        .filter_map(|terminal| {
            let at = Timestamp::new(
                terminal.committed_at_ms / 1_000,
                u32::try_from(terminal.committed_at_ms % 1_000).unwrap_or(0) * 1_000_000,
            )
            .ok()?;
            Some(ProductObservation::TurnUsage {
                at,
                session_id: terminal.session_id,
                branch_id: terminal.branch_id,
                turn_id: terminal.turn_id,
                usage: terminal.usage,
            })
        })
        .collect();
    if observations.is_empty() {
        return;
    }
    if let Some(writer) = crate::telemetry_sink::telemetry_writer_for_home(paths.home()) {
        for observation in observations {
            writer.emit_product(observation);
        }
    } else {
        // Lease-free CLI passes can apply before a runtime attaches its sink.
        let batch: Vec<_> = observations
            .into_iter()
            .map(TelemetryItem::Product)
            .collect();
        let result = TelemetryStore::open(paths)
            .map_err(|error| error.to_string())
            .and_then(|mut store| store.write_batch(&batch).map_err(|error| error.to_string()));
        if let Err(error) = result {
            tracing::warn!(%error, "projected terminal usage observation was lost");
        }
    }
}

/// Row counts returned by `plak telemetry status`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TelemetryStoreStatus {
    pub schema_version: i64,
    pub table_counts: Vec<(String, u64)>,
}

const TELEMETRY_TABLES: &[&str] = &[
    "turns",
    "model_attempts",
    "tool_calls",
    "compactions",
    "durability_attempts",
    "retention_runs",
    "product_events",
    "resource_samples",
    "model_retries",
    "credential_refreshes",
    "job_events",
    "prompt_cache_diagnoses",
];

const SESSION_PRUNE_TABLES: &[&str] = &[
    "turns",
    "model_attempts",
    "tool_calls",
    "compactions",
    "durability_attempts",
    "retention_runs",
    "product_events",
    "resource_samples",
    "model_retries",
    "job_events",
    "prompt_cache_diagnoses",
];

/// Typed failure opening the disposable telemetry database.
#[derive(Debug)]
pub enum TelemetryStoreOpenError {
    /// Product layout prerequisites could not be established.
    Layout(String),
    /// The database could not be opened or configured.
    Unavailable(String),
    /// The file has a schema this build does not write or read.
    SchemaMismatch { found: i64, expected: i64 },
    /// The dedicated telemetry writer thread could not start.
    WriterThread { source: std::io::Error },
}

impl TelemetryStoreOpenError {
    fn layout(message: impl Into<String>) -> Self {
        Self::Layout(message.into())
    }

    fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable(message.into())
    }

    fn schema_mismatch(found: i64) -> Self {
        Self::SchemaMismatch {
            found,
            expected: TELEMETRY_SCHEMA_VERSION,
        }
    }
}

impl std::fmt::Display for TelemetryStoreOpenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Layout(message) | Self::Unavailable(message) => message.fmt(formatter),
            Self::SchemaMismatch { found, expected } => write!(
                formatter,
                "telemetry store schema version {found} is unsupported; the file is left untouched (expected {expected}); removal is explicit"
            ),
            Self::WriterThread { source } => {
                write!(
                    formatter,
                    "telemetry writer thread could not start: {source}"
                )
            }
        }
    }
}

impl std::error::Error for TelemetryStoreOpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::WriterThread { source } => Some(source),
            _ => None,
        }
    }
}

/// SQLite-backed telemetry store accessed only from the dedicated writer thread.
pub struct TelemetryStore {
    connection: Connection,
}

impl TelemetryStore {
    pub fn open(paths: &PlakPaths) -> Result<Self, TelemetryStoreOpenError> {
        let _span = tracing::info_span!(
            "telemetry_store.open",
            schema_version = TELEMETRY_SCHEMA_VERSION
        )
        .entered();
        paths
            .ensure_layout()
            .map_err(TelemetryStoreOpenError::layout)?;
        let database = paths.telemetry_database();
        let connection = Connection::open_with_flags(
            &database,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|error| {
            TelemetryStoreOpenError::unavailable(format!("could not open telemetry store: {error}"))
        })?;
        connection.busy_timeout(BUSY_TIMEOUT).map_err(|_| {
            TelemetryStoreOpenError::unavailable("could not configure telemetry timeout")
        })?;
        let mut store = Self { connection };
        store.admit_schema()?;
        let connection = &store.connection;
        let mode: String = connection
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .map_err(|error| {
                TelemetryStoreOpenError::unavailable(format!(
                    "could not enable telemetry store WAL: {error}"
                ))
            })?;
        if mode != "wal" {
            return Err(TelemetryStoreOpenError::layout(
                "telemetry store refused WAL mode",
            ));
        }
        connection
            .execute_batch("PRAGMA synchronous = NORMAL;")
            .map_err(|_| {
                TelemetryStoreOpenError::unavailable("could not configure telemetry store")
            })?;
        let synchronous: i64 = connection
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .map_err(|_| {
                TelemetryStoreOpenError::unavailable("could not inspect telemetry store")
            })?;
        if synchronous != 1 {
            return Err(TelemetryStoreOpenError::layout(
                "telemetry store durability configuration was not retained",
            ));
        }
        configure_journal_size_limit(connection, DEFAULT_TELEMETRY_MAX_BYTES).map_err(|_| {
            TelemetryStoreOpenError::unavailable("could not configure telemetry WAL size limit")
        })?;
        let journal_limit: i64 = connection
            .query_row("PRAGMA journal_size_limit", [], |row| row.get(0))
            .map_err(|_| {
                TelemetryStoreOpenError::unavailable("could not inspect telemetry WAL size limit")
            })?;
        let expected_limit = i64::try_from(DEFAULT_TELEMETRY_MAX_BYTES).unwrap_or(i64::MAX);
        if journal_limit != expected_limit {
            return Err(TelemetryStoreOpenError::layout(
                "telemetry store WAL size limit was not retained",
            ));
        }
        Ok(store)
    }

    pub fn write_batch(&mut self, items: &[TelemetryItem]) -> Result<(), rusqlite::Error> {
        if items.is_empty() {
            return Ok(());
        }
        let transaction = self.connection.transaction()?;
        for item in items {
            if let Err(error) = write_item(&transaction, item) {
                let _ = transaction.rollback();
                return Err(error);
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Delete whole sessions oldest-first until under caps, then VACUUM and
    /// truncate the WAL. Stop byte pruning when a reader prevents reclamation.
    /// Sessions with unfinished turns are retained, including other writers.
    pub fn prune(
        &mut self,
        retention: TelemetryRetentionConfig,
        database: &Path,
    ) -> Result<TelemetryPruneOutcome, rusqlite::Error> {
        configure_journal_size_limit(&self.connection, retention.max_bytes)?;
        let mut checkpoint = checkpoint_wal_truncate(&self.connection)?;
        if checkpoint.is_busy() {
            return Ok(TelemetryPruneOutcome {
                wal_checkpoint: checkpoint,
            });
        }
        let expired = prune_expired_sessions(&self.connection, retention.max_age_days)?;
        if expired > 0 {
            self.connection.execute("VACUUM", [])?;
            checkpoint = checkpoint.and(checkpoint_wal_truncate(&self.connection)?);
        }
        let mut passes = 0_u32;
        while !checkpoint.is_busy()
            && TelemetryStore::database_size_bytes(database) > retention.max_bytes
            && passes < MAX_BYTE_PRUNE_PASSES
        {
            let Some((session_id, _)) = oldest_session(&self.connection)? else {
                break;
            };
            delete_session_rows(&self.connection, &session_id)?;
            self.connection.execute("VACUUM", [])?;
            checkpoint = checkpoint.and(checkpoint_wal_truncate(&self.connection)?);
            passes = passes.saturating_add(1);
        }
        Ok(TelemetryPruneOutcome {
            wal_checkpoint: checkpoint,
        })
    }

    /// Truncate the WAL without waiting on readers. Busy is a reported outcome.
    pub fn checkpoint_wal(&self) -> Result<WalCheckpointOutcome, rusqlite::Error> {
        checkpoint_wal_truncate(&self.connection)
    }

    /// Read-only status for `plak telemetry status`.
    pub fn status_snapshot(&self) -> Result<TelemetryStoreStatus, rusqlite::Error> {
        let schema_version: i64 = self
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let mut table_counts = Vec::new();
        for table in TELEMETRY_TABLES {
            let count: i64 =
                self.connection
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })?;
            table_counts.push((
                (*table).to_owned(),
                u64::try_from(count).unwrap_or(u64::MAX),
            ));
        }
        Ok(TelemetryStoreStatus {
            schema_version,
            table_counts,
        })
    }

    /// Run read-only SQL with product/runtime attached (for CLI).
    pub fn query_attached(
        paths: &PlakPaths,
        sql: &str,
    ) -> Result<Vec<Vec<String>>, TelemetryStoreOpenError> {
        let database = paths.telemetry_database();
        if !database.exists() {
            return Err(TelemetryStoreOpenError::unavailable(
                "telemetry database does not exist",
            ));
        }
        let connection = Connection::open_with_flags(
            &database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|error| {
            TelemetryStoreOpenError::unavailable(format!("could not open telemetry store: {error}"))
        })?;
        TelemetryStore::require_attached_schema(&connection)?;
        let product_attached = attach_readonly(&connection, "product", &paths.product_database())?;
        attach_readonly(&connection, "runtime", &paths.runtime_database())?;
        let subagents_attached =
            attach_readonly(&connection, "subagents", &paths.subagents_database())?;
        if product_attached {
            create_session_models_view(&connection, subagents_attached)?;
        }
        let mut statement = connection
            .prepare(sql)
            .map_err(|error| TelemetryStoreOpenError::unavailable(error.to_string()))?;
        let column_count = statement.column_count();
        let mut rows = Vec::new();
        let mapped = statement
            .query_map([], |row| {
                let mut values = Vec::with_capacity(column_count);
                for index in 0..column_count {
                    let value: rusqlite::types::Value = row.get(index)?;
                    values.push(sqlite_value_text(value));
                }
                Ok(values)
            })
            .map_err(|error| TelemetryStoreOpenError::unavailable(error.to_string()))?;
        for row in mapped {
            rows.push(
                row.map_err(|error| TelemetryStoreOpenError::unavailable(error.to_string()))?,
            );
        }
        Ok(rows)
    }

    /// File lengths of the main database plus `-wal` and `-shm` if present.
    #[must_use]
    pub fn database_size_bytes(database: &Path) -> u64 {
        sqlite_file_len(database)
            .saturating_add(sqlite_file_len(&sqlite_sidecar(database, "-wal")))
            .saturating_add(sqlite_file_len(&sqlite_sidecar(database, "-shm")))
    }

    fn admit_schema(&mut self) -> Result<(), TelemetryStoreOpenError> {
        let version: i64 = self
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|error| TelemetryStoreOpenError::unavailable(error.to_string()))?;
        if version == TELEMETRY_SCHEMA_VERSION {
            return Ok(());
        }
        if version != 0 {
            return Err(TelemetryStoreOpenError::schema_mismatch(version));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| TelemetryStoreOpenError::unavailable(error.to_string()))?;
        // A concurrent first writer may have initialized the file while we waited.
        let version: i64 = transaction
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|error| TelemetryStoreOpenError::unavailable(error.to_string()))?;
        if version == TELEMETRY_SCHEMA_VERSION {
            return transaction
                .commit()
                .map_err(|error| TelemetryStoreOpenError::unavailable(error.to_string()));
        }
        let objects: i64 = transaction
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )
            .map_err(|error| TelemetryStoreOpenError::unavailable(error.to_string()))?;
        if version != 0 || objects != 0 {
            return Err(TelemetryStoreOpenError::schema_mismatch(version));
        }
        transaction
            .execute_batch(SCHEMA_DDL)
            .and_then(|()| {
                transaction.pragma_update(None, "user_version", TELEMETRY_SCHEMA_VERSION)
            })
            .and_then(|()| transaction.commit())
            .map_err(|error| TelemetryStoreOpenError::unavailable(error.to_string()))?;
        Ok(())
    }

    fn require_attached_schema(connection: &Connection) -> Result<(), TelemetryStoreOpenError> {
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|error| {
                TelemetryStoreOpenError::unavailable(format!(
                    "could not read telemetry schema version: {error}"
                ))
            })?;
        if version != TELEMETRY_SCHEMA_VERSION {
            return Err(TelemetryStoreOpenError::schema_mismatch(version));
        }
        Ok(())
    }

    /// Read-only row count across all telemetry tables for doctor diagnostics.
    pub fn count_all_rows(paths: &PlakPaths) -> Result<u64, TelemetryStoreOpenError> {
        let database = paths.telemetry_database();
        if !database.exists() {
            return Ok(0);
        }
        let connection = Connection::open_with_flags(
            &database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|error| {
            TelemetryStoreOpenError::unavailable(format!(
                "could not inspect telemetry store: {error}"
            ))
        })?;
        TelemetryStore::require_attached_schema(&connection)?;
        let mut total = 0_u64;
        for table in TELEMETRY_TABLES {
            let count: i64 = connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .map_err(|error| {
                    TelemetryStoreOpenError::unavailable(format!(
                        "could not count telemetry rows: {error}"
                    ))
                })?;
            total = total.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        }
        Ok(total)
    }

    /// Writes agent observations synchronously (integration tests only).
    #[cfg(any(test, feature = "test-support"))]
    pub fn write_observations_for_test(
        &mut self,
        observations: &[TelemetryObservation],
    ) -> Result<(), rusqlite::Error> {
        let items: Vec<_> = observations
            .iter()
            .copied()
            .map(TelemetryItem::Agent)
            .collect();
        self.write_batch(&items)
    }

    #[cfg(test)]
    pub fn count_rows(&self, table: &str) -> u64 {
        u64::try_from(
            self.connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap_or(0),
        )
        .unwrap_or(0)
    }

    #[cfg(test)]
    pub fn query_i64(&self, sql: &str) -> Option<i64> {
        self.connection.query_row(sql, [], |row| row.get(0)).ok()
    }

    #[cfg(test)]
    pub fn query_text(&self, sql: &str) -> Option<String> {
        self.connection.query_row(sql, [], |row| row.get(0)).ok()
    }
}

fn write_item(
    transaction: &rusqlite::Transaction<'_>,
    item: &TelemetryItem,
) -> Result<(), rusqlite::Error> {
    match item {
        TelemetryItem::Agent(observation) => write_observation(transaction, observation),
        TelemetryItem::Product(observation) => write_product_observation(transaction, observation),
    }
}

/// Attaches `database` read-only as `alias`; returns whether it was attached.
fn attach_readonly(
    connection: &Connection,
    alias: &str,
    database: &Path,
) -> Result<bool, TelemetryStoreOpenError> {
    if !database.exists() {
        return Ok(false);
    }
    let uri = format!(
        "file:{}?mode=ro",
        database.to_string_lossy().replace('\\', "/")
    );
    connection
        .execute_batch(&format!("ATTACH DATABASE '{uri}' AS {alias}"))
        .map_err(|error| TelemetryStoreOpenError::unavailable(error.to_string()))?;
    Ok(true)
}

/// Telemetry rows carry the plakous `session_id`; the product tree carries
/// `SessionLocator` strings (`session/branch/checkpoint`) on root sessions and
/// on spawned child nodes. Product `agent_nodes.agent_id` is a tree node id,
/// not the plakous agent id, so model provenance must join on the locator's
/// session prefix. Persistent views cannot reference an attached schema, hence
/// a TEMP view built after ATTACH.
pub const SESSION_MODELS_VIEW: &str = "v_session_models";

fn create_session_models_view(
    connection: &Connection,
    subagents_attached: bool,
) -> Result<(), TelemetryStoreOpenError> {
    let children = if subagents_attached {
        "UNION SELECT substr(session_locator, 1, instr(session_locator, '/') - 1),
                    tuple_id, account, effort, NULL, 'child'
             FROM subagents.agent_nodes
             WHERE session_locator IS NOT NULL AND instr(session_locator, '/') > 1"
    } else {
        ""
    };
    connection
        .execute_batch(&format!(
            "CREATE TEMP VIEW {SESSION_MODELS_VIEW} AS
             SELECT substr(locator, 1, instr(locator, '/') - 1) AS session_id,
                    model AS tuple_id, account, effort, composition, 'root' AS origin
             FROM product.sessions
             WHERE locator IS NOT NULL AND instr(locator, '/') > 1
             {children}"
        ))
        .map_err(|error| TelemetryStoreOpenError::unavailable(error.to_string()))
}

fn sqlite_value_text(value: rusqlite::types::Value) -> String {
    match value {
        rusqlite::types::Value::Null => String::new(),
        rusqlite::types::Value::Integer(v) => v.to_string(),
        rusqlite::types::Value::Real(v) => v.to_string(),
        rusqlite::types::Value::Text(v) => v,
        rusqlite::types::Value::Blob(v) => format!("<blob {} bytes>", v.len()),
    }
}

fn sqlite_sidecar(database: &Path, suffix: &str) -> PathBuf {
    let mut path = database.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

fn sqlite_file_len(path: &Path) -> u64 {
    std::fs::metadata(path)
        .ok()
        .map(|meta| meta.len())
        .unwrap_or(0)
}

fn configure_journal_size_limit(
    connection: &Connection,
    max_bytes: u64,
) -> Result<(), rusqlite::Error> {
    let limit = i64::try_from(max_bytes).unwrap_or(i64::MAX);
    connection.pragma_update(None, "journal_size_limit", limit)
}

fn checkpoint_wal_truncate(
    connection: &Connection,
) -> Result<WalCheckpointOutcome, rusqlite::Error> {
    connection.busy_timeout(CHECKPOINT_BUSY_TIMEOUT)?;
    let checkpoint = connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
        let busy: i64 = row.get(0)?;
        Ok(if busy == 0 {
            WalCheckpointOutcome::Completed
        } else {
            WalCheckpointOutcome::Busy
        })
    });
    let restored = connection.busy_timeout(BUSY_TIMEOUT);
    match checkpoint {
        Ok(WalCheckpointOutcome::Busy) => {
            warn_wal_checkpoint_busy();
            restored?;
            Ok(WalCheckpointOutcome::Busy)
        }
        Ok(completed) => {
            restored?;
            Ok(completed)
        }
        Err(error) if sqlite_lock_busy(&error) => {
            warn_wal_checkpoint_busy();
            restored?;
            Ok(WalCheckpointOutcome::Busy)
        }
        Err(error) => {
            let _ = restored;
            Err(error)
        }
    }
}

fn warn_wal_checkpoint_busy() {
    tracing::warn!("telemetry WAL checkpoint TRUNCATE busy; a reader still holds the log");
}

fn sqlite_lock_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

fn age_cutoff_ns(max_age_days: u64) -> i64 {
    // Config refuses `max_age_days == 0`, so the cutoff is always finite.
    let age_seconds = i64::try_from(max_age_days.saturating_mul(86_400)).unwrap_or(i64::MAX);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| {
            i64::try_from(duration.as_secs())
                .unwrap_or(i64::MAX)
                .saturating_mul(1_000_000_000)
        })
        .unwrap_or(i64::MAX);
    now.saturating_sub(age_seconds.saturating_mul(1_000_000_000))
}

fn sessions_by_oldest(connection: &Connection) -> Result<Vec<(String, i64)>, rusqlite::Error> {
    let mut statement = connection.prepare(
        "SELECT session_id, MIN(activity_ns) AS oldest FROM (
            SELECT session_id, COALESCE(started_at, finished_at) AS activity_ns FROM turns
            UNION ALL SELECT session_id, at FROM product_events
            UNION ALL SELECT session_id, started_at FROM compactions WHERE started_at IS NOT NULL
            UNION ALL SELECT session_id, finished_at FROM compactions WHERE finished_at IS NOT NULL
            UNION ALL SELECT session_id, at FROM resource_samples
            UNION ALL SELECT session_id, at FROM model_retries
            UNION ALL SELECT session_id, at FROM job_events
            UNION ALL SELECT session_id, at FROM prompt_cache_diagnoses
         ) WHERE session_id NOT IN (
             SELECT session_id FROM turns WHERE finished_at IS NULL AND status IS NULL
         ) GROUP BY session_id ORDER BY oldest ASC",
    )?;
    let sessions = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(sessions)
}

fn oldest_session(connection: &Connection) -> Result<Option<(String, i64)>, rusqlite::Error> {
    Ok(sessions_by_oldest(connection)?.into_iter().next())
}

fn prune_expired_sessions(
    connection: &Connection,
    max_age_days: u64,
) -> Result<u64, rusqlite::Error> {
    let cutoff_ns = age_cutoff_ns(max_age_days);
    let mut deleted = 0_u64;
    for (session_id, oldest) in sessions_by_oldest(connection)? {
        if oldest >= cutoff_ns {
            break;
        }
        delete_session_rows(connection, &session_id)?;
        deleted = deleted.saturating_add(1);
    }
    let credentials = connection.execute(
        "DELETE FROM credential_refreshes WHERE at < ?1",
        [cutoff_ns],
    )?;
    Ok(deleted.saturating_add(u64::try_from(credentials).unwrap_or(u64::MAX)))
}

fn delete_session_rows(connection: &Connection, session_id: &str) -> Result<(), rusqlite::Error> {
    for table in SESSION_PRUNE_TABLES {
        connection.execute(
            &format!("DELETE FROM {table} WHERE session_id = ?1"),
            [session_id],
        )?;
    }
    Ok(())
}

fn write_product_observation(
    transaction: &rusqlite::Transaction<'_>,
    observation: &ProductObservation,
) -> Result<(), rusqlite::Error> {
    let at = timestamp_ns(observation.at());
    let session_id = id_text(observation.session_id());
    match *observation {
        ProductObservation::TurnUsage {
            branch_id,
            turn_id,
            usage,
            ..
        } => {
            let model = usage.model();
            let (cost_micros, cost_status) = cost_fields_from_usage(model);
            transaction.execute(
                "INSERT INTO turns (
                    session_id, branch_id, run_id, agent_id, turn_id, finished_at,
                    input_tokens, output_tokens, reasoning_tokens, cached_input_tokens,
                    cache_write_tokens, cost_micros, cost_status
                 ) VALUES (?1, ?2, '', '', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(turn_id) DO UPDATE SET
                    finished_at = excluded.finished_at,
                    input_tokens = excluded.input_tokens,
                    output_tokens = excluded.output_tokens,
                    reasoning_tokens = excluded.reasoning_tokens,
                    cached_input_tokens = excluded.cached_input_tokens,
                    cache_write_tokens = excluded.cache_write_tokens,
                    cost_micros = excluded.cost_micros,
                    cost_status = excluded.cost_status",
                params![
                    session_id,
                    id_text(branch_id),
                    id_text(turn_id),
                    at,
                    i64::try_from(model.input_tokens()).unwrap_or(i64::MAX),
                    i64::try_from(model.output_tokens()).unwrap_or(i64::MAX),
                    i64::try_from(model.reasoning_tokens()).unwrap_or(i64::MAX),
                    i64::try_from(model.cached_input_tokens()).unwrap_or(i64::MAX),
                    i64::try_from(model.cache_write_tokens()).unwrap_or(i64::MAX),
                    cost_micros,
                    cost_status
                ],
            )?;
        }

        ProductObservation::RunBoundExit { bound, .. } => {
            transaction.execute("INSERT INTO product_events (at, kind, session_id, status) VALUES (?1, 'run_bound_exit', ?2, ?3)",
                params![at, session_id, bound.as_str()])?;
        }
        ProductObservation::TuiTiming {
            phase,
            resumed,
            elapsed,
            ..
        } => {
            transaction.execute(
                "INSERT INTO product_events (at, kind, session_id, status, elapsed_ns)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    at,
                    phase.as_str(),
                    session_id,
                    if resumed { "resume" } else { "new" },
                    duration_ns(elapsed)
                ],
            )?;
        }
        ProductObservation::ModelRetry {
            branch_id,
            agent_id,
            turn_id,
            next_attempt,
            max_attempts,
            cause,
            loss,
            ..
        } => {
            let phase = loss
                .transport_phase
                .map_or("unknown", plakous::model::ModelRetryPhase::as_str);
            transaction.execute(
                "INSERT INTO model_retries (at, session_id, branch_id, agent_id, turn_id, next_attempt, max_attempts, cause_kind, transport_phase, assistant_text_bytes, reasoning_bytes, completed_items, elapsed_ms, replay_safety, declined_reason) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![at, session_id, id_text(branch_id), id_text(agent_id), id_text(turn_id), next_attempt, max_attempts, cause.kind(), phase, i64::try_from(loss.assistant_text_bytes).unwrap_or(i64::MAX), i64::try_from(loss.reasoning_bytes).unwrap_or(i64::MAX), i64::try_from(loss.completed_items).unwrap_or(i64::MAX), i64::try_from(loss.elapsed_ms).unwrap_or(i64::MAX), if loss.server_effect { "server_effect" } else { "cost_only" }, loss.declined_reason.map(plakous::model::ModelRetryDeclineReason::as_str)],
            )?;
        }
        ProductObservation::TreeNodeSpawned {
            tree_id,
            node_id,
            parent_node_id,
            template,
            ..
        } => {
            transaction.execute(
                "INSERT INTO product_events (
                    at, kind, session_id, tree_id, node_id, parent_node_id, spawn_class
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    at,
                    "tree_node_spawned",
                    session_id,
                    tree_id.as_str(),
                    node_id.as_str(),
                    parent_node_id.map(|value| value.as_str().to_owned()),
                    template.as_str(),
                ],
            )?;
        }
        ProductObservation::TreeNodeAttached {
            tree_id,
            node_id,
            parent_node_id,
            elapsed,
            ..
        } => {
            transaction.execute(
                "INSERT INTO product_events (
                    at, kind, session_id, tree_id, node_id, parent_node_id, elapsed_ns
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    at,
                    "tree_node_attached",
                    session_id,
                    tree_id.as_str(),
                    node_id.as_str(),
                    parent_node_id.map(|value| value.as_str().to_owned()),
                    duration_ns(elapsed),
                ],
            )?;
        }
        ProductObservation::TreeNodeDetached {
            tree_id,
            node_id,
            parent_node_id,
            status,
            ..
        } => {
            transaction.execute(
                "INSERT INTO product_events (
                    at, kind, session_id, tree_id, node_id, parent_node_id, status
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    at,
                    "tree_node_detached",
                    session_id,
                    tree_id.as_str(),
                    node_id.as_str(),
                    parent_node_id.map(|value| value.as_str().to_owned()),
                    status.as_str(),
                ],
            )?;
        }
        ProductObservation::TreeRebound { tree_id, .. } => {
            transaction.execute(
                "INSERT INTO product_events (at, kind, session_id, tree_id)
                 VALUES (?1, ?2, ?3, ?4)",
                params![at, "tree_rebound", session_id, tree_id.as_str()],
            )?;
        }
        ProductObservation::TreeEpochAdvanced { tree_id, .. } => {
            transaction.execute(
                "INSERT INTO product_events (at, kind, session_id, tree_id)
                 VALUES (?1, ?2, ?3, ?4)",
                params![at, "tree_epoch_advanced", session_id, tree_id.as_str()],
            )?;
        }
        ProductObservation::JsonlCommand {
            kind,
            admitted_to_completed,
            ..
        } => {
            transaction.execute(
                "INSERT INTO product_events (at, kind, session_id, spawn_class, elapsed_ns)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    at,
                    "jsonl_command",
                    session_id,
                    kind.as_str(),
                    duration_ns(admitted_to_completed),
                ],
            )?;
        }
        ProductObservation::SessionOpened { elapsed, .. } => {
            transaction.execute(
                "INSERT INTO product_events (at, kind, session_id, elapsed_ns)
                 VALUES (?1, ?2, ?3, ?4)",
                params![at, "session_opened", session_id, duration_ns(elapsed)],
            )?;
        }
        ProductObservation::SessionResumed { elapsed, .. } => {
            transaction.execute(
                "INSERT INTO product_events (at, kind, session_id, elapsed_ns)
                 VALUES (?1, ?2, ?3, ?4)",
                params![at, "session_resumed", session_id, duration_ns(elapsed)],
            )?;
        }
        ProductObservation::ResourceSample {
            agent_id,
            phase,
            child_kind,
            rss_bytes,
            peak_rss_bytes,
            user_cpu,
            system_cpu,
            threads,
            fds,
            ..
        } => {
            transaction.execute(
                "INSERT INTO resource_samples (
                    at, session_id, agent_id, phase, child_kind, rss_bytes, peak_rss_bytes,
                    user_cpu_ns, system_cpu_ns, threads, fds
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    at,
                    session_id,
                    agent_id.map(id_text),
                    phase.as_str(),
                    child_kind.map(ChildKind::as_str),
                    rss_bytes.map(i64::try_from).transpose().ok().flatten(),
                    peak_rss_bytes.map(i64::try_from).transpose().ok().flatten(),
                    duration_ns(user_cpu),
                    duration_ns(system_cpu),
                    threads.map(i64::try_from).transpose().ok().flatten(),
                    fds.map(i64::try_from).transpose().ok().flatten(),
                ],
            )?;
        }
    }
    Ok(())
}

fn write_observation(
    transaction: &rusqlite::Transaction<'_>,
    observation: &TelemetryObservation,
) -> Result<(), rusqlite::Error> {
    let context = observation.context();
    let timestamp = timestamp_ns(observation.timestamp());
    match observation.kind() {
        TelemetryObservationKind::JobPromoted { .. }
        | TelemetryObservationKind::JobWaited { .. }
        | TelemetryObservationKind::JobPreempted { .. }
        | TelemetryObservationKind::JobNoticesDelivered { .. } => {
            let (kind, tool, nested, source, hint, point, count) = match observation.kind() {
                TelemetryObservationKind::JobPromoted {
                    tool,
                    nested,
                    hint_source,
                    hint_ms,
                } => (
                    "promoted",
                    Some(tool.as_str()),
                    Some(nested),
                    Some(hint_source.as_str()),
                    Some(hint_ms),
                    None,
                    None,
                ),
                TelemetryObservationKind::JobWaited { tool } => {
                    ("waited", Some(tool.as_str()), None, None, None, None, None)
                }
                TelemetryObservationKind::JobPreempted { kind } => {
                    (kind.as_str(), None, None, None, None, None, None)
                }
                TelemetryObservationKind::JobNoticesDelivered { point, count } => (
                    "notices",
                    None,
                    None,
                    None,
                    None,
                    Some(point.as_str()),
                    Some(count),
                ),
                _ => unreachable!("jobs observation matched above"),
            };
            transaction.execute(
                "INSERT INTO job_events (at, session_id, branch_id, run_id, agent_id, turn_id, call_id, kind, tool, nested, hint_source, hint_ms, delivery_point, count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![timestamp, id_text(context.session_id()), id_text(context.branch_id()),
                    id_text(context.run_id()), id_text(context.agent_id()), context.turn_id().map(id_text),
                    context.call_id().map(id_text), kind, tool, nested, source,
                    hint.map(|value| i64::try_from(value).unwrap_or(i64::MAX)), point, count],
            )?;
        }
        TelemetryObservationKind::TurnStarted { queue_time, origin } => {
            let Some(turn_id) = context.turn_id() else {
                return Ok(());
            };
            transaction.execute(
                "INSERT INTO turns (
                    session_id, branch_id, run_id, agent_id, turn_id,
                    started_at, queue_ns, origin
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(turn_id) DO UPDATE SET
                    started_at = excluded.started_at,
                    run_id = CASE WHEN turns.run_id = '' THEN excluded.run_id ELSE turns.run_id END,
                    agent_id = CASE WHEN turns.agent_id = '' THEN excluded.agent_id ELSE turns.agent_id END,
                    queue_ns = excluded.queue_ns,
                    origin = excluded.origin",
                params![
                    id_text(context.session_id()),
                    id_text(context.branch_id()),
                    id_text(context.run_id()),
                    id_text(context.agent_id()),
                    id_text(turn_id),
                    timestamp,
                    duration_ns(queue_time),
                    origin.as_str(),
                ],
            )?;
        }
        TelemetryObservationKind::TurnFinished {
            elapsed,
            status,
            model_wall,
            tool_wall,
            compaction_wall,
            model_attempts,
            tool_calls,
            compactions,
            ..
        } => {
            let Some(turn_id) = context.turn_id() else {
                return Ok(());
            };
            transaction.execute(
                "INSERT INTO turns (
                    session_id, branch_id, run_id, agent_id, turn_id,
                    elapsed_ns, status, model_wall_ns, tool_wall_ns, compaction_wall_ns,
                    model_attempts, tool_calls, compactions
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT(turn_id) DO UPDATE SET
                    run_id = CASE WHEN turns.run_id = '' THEN excluded.run_id ELSE turns.run_id END,
                    agent_id = CASE WHEN turns.agent_id = '' THEN excluded.agent_id ELSE turns.agent_id END,
                    elapsed_ns = excluded.elapsed_ns,
                    status = excluded.status,
                    model_wall_ns = excluded.model_wall_ns,
                    tool_wall_ns = excluded.tool_wall_ns,
                    compaction_wall_ns = excluded.compaction_wall_ns,
                    model_attempts = excluded.model_attempts,
                    tool_calls = excluded.tool_calls,
                    compactions = excluded.compactions",
                params![
                    id_text(context.session_id()),
                    id_text(context.branch_id()),
                    id_text(context.run_id()),
                    id_text(context.agent_id()),
                    id_text(turn_id),
                    duration_ns(elapsed),
                    terminal_status_text(status),
                    duration_ns(model_wall),
                    duration_ns(tool_wall),
                    duration_ns(compaction_wall),
                    i64::from(model_attempts),
                    i64::from(tool_calls),
                    i64::from(compactions)
                ],
            )?;
        }
        TelemetryObservationKind::ModelStarted => {
            let (Some(turn_id), Some(attempt_id)) = (context.turn_id(), context.attempt_id())
            else {
                return Ok(());
            };
            transaction.execute(
                "INSERT INTO model_attempts (
                    attempt_id, session_id, branch_id, run_id, agent_id, turn_id, started_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(attempt_id) DO UPDATE SET
                    started_at = excluded.started_at",
                params![
                    id_text(attempt_id),
                    id_text(context.session_id()),
                    id_text(context.branch_id()),
                    id_text(context.run_id()),
                    id_text(context.agent_id()),
                    id_text(turn_id),
                    timestamp,
                ],
            )?;
        }
        TelemetryObservationKind::ModelFirstEvent { elapsed } => {
            let Some(attempt_id) = context.attempt_id() else {
                return Ok(());
            };
            transaction.execute(
                "INSERT INTO model_attempts (
                    attempt_id, session_id, branch_id, run_id, agent_id, turn_id,
                    first_event_at, ttft_ns
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(attempt_id) DO UPDATE SET
                    first_event_at = excluded.first_event_at,
                    ttft_ns = excluded.ttft_ns",
                params![
                    id_text(attempt_id),
                    id_text(context.session_id()),
                    id_text(context.branch_id()),
                    id_text(context.run_id()),
                    id_text(context.agent_id()),
                    context.turn_id().map(id_text).unwrap_or_default(),
                    timestamp,
                    duration_ns(elapsed),
                ],
            )?;
        }
        TelemetryObservationKind::ModelStreamActivity { activity } => {
            let Some(attempt_id) = context.attempt_id() else {
                return Ok(());
            };
            let semantic_at = activity.last_semantic_event_elapsed_ms.map(|_| {
                timestamp.saturating_sub(
                    i64::try_from(activity.semantic_gap_ms.saturating_mul(1_000_000))
                        .unwrap_or(i64::MAX),
                )
            });
            transaction.execute(
                "INSERT INTO model_attempts (attempt_id, session_id, branch_id, run_id, agent_id, turn_id, last_semantic_event_at, keepalive_count, max_semantic_gap_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(attempt_id) DO UPDATE SET last_semantic_event_at = COALESCE(excluded.last_semantic_event_at, model_attempts.last_semantic_event_at), keepalive_count = MAX(model_attempts.keepalive_count, excluded.keepalive_count), max_semantic_gap_ms = MAX(model_attempts.max_semantic_gap_ms, excluded.max_semantic_gap_ms)",
                params![id_text(attempt_id), id_text(context.session_id()), id_text(context.branch_id()), id_text(context.run_id()), id_text(context.agent_id()), context.turn_id().map(id_text).unwrap_or_default(), semantic_at, i64::try_from(activity.keepalive_count).unwrap_or(i64::MAX), i64::try_from(activity.max_semantic_gap_ms).unwrap_or(i64::MAX)],
            )?;
        }
        TelemetryObservationKind::ModelFinished {
            elapsed,
            status,
            usage,
            served_by,
            output_deltas,
        } => {
            let Some(attempt_id) = context.attempt_id() else {
                return Ok(());
            };
            let (input, output, reasoning, cached_input, cache_write) = usage
                .map(usage_columns)
                .unwrap_or((None, None, None, None, None));
            let (cost_micros, cost_status) =
                usage.map_or((None, "unknown"), cost_fields_from_usage);
            // Opaque observational token, not a provider or admission classification.
            let fingerprint = usage
                .map(Usage::output_tokens)
                .filter(|tokens| *tokens > 0)
                .map(|tokens| {
                    let per_mille = u128::from(output_deltas) * 1000 / u128::from(tokens);
                    format!("dpt-v1:{per_mille}")
                });
            let served_endpoint = served_by.map(|value| value.as_str().to_owned());
            transaction.execute(
                "INSERT INTO model_attempts (
                    attempt_id, session_id, branch_id, run_id, agent_id, turn_id,
                    finished_at, ttlb_ns, status,
                    input_tokens, output_tokens, reasoning_tokens,
                    cached_input_tokens, cache_write_tokens,
                    served_endpoint, cost_micros, cost_status, served_path_fingerprint
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
                 ON CONFLICT(attempt_id) DO UPDATE SET
                    finished_at = excluded.finished_at,
                    ttlb_ns = excluded.ttlb_ns,
                    status = excluded.status,
                    input_tokens = excluded.input_tokens,
                    output_tokens = excluded.output_tokens,
                    reasoning_tokens = excluded.reasoning_tokens,
                    cached_input_tokens = excluded.cached_input_tokens,
                    cache_write_tokens = excluded.cache_write_tokens,
                    served_endpoint = excluded.served_endpoint,
                    cost_micros = excluded.cost_micros,
                    cost_status = excluded.cost_status,
                    served_path_fingerprint = excluded.served_path_fingerprint",
                params![
                    id_text(attempt_id),
                    id_text(context.session_id()),
                    id_text(context.branch_id()),
                    id_text(context.run_id()),
                    id_text(context.agent_id()),
                    context.turn_id().map(id_text).unwrap_or_default(),
                    timestamp,
                    duration_ns(elapsed),
                    terminal_status_text(status),
                    input,
                    output,
                    reasoning,
                    cached_input,
                    cache_write,
                    served_endpoint,
                    cost_micros,
                    cost_status,
                    fingerprint,
                ],
            )?;
        }
        TelemetryObservationKind::ToolStarted { tool } => {
            let (Some(turn_id), Some(call_id)) = (context.turn_id(), context.call_id()) else {
                return Ok(());
            };
            transaction.execute(
                "INSERT INTO tool_calls (
                    call_id, session_id, branch_id, run_id, agent_id, turn_id,
                    attempt_id, started_at, tool_name
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(turn_id, call_id) DO UPDATE SET
                    started_at = excluded.started_at,
                    tool_name = excluded.tool_name",
                params![
                    id_text(call_id),
                    id_text(context.session_id()),
                    id_text(context.branch_id()),
                    id_text(context.run_id()),
                    id_text(context.agent_id()),
                    id_text(turn_id),
                    context.attempt_id().map(id_text),
                    timestamp,
                    tool.as_str(),
                ],
            )?;
        }
        TelemetryObservationKind::ToolFinished {
            elapsed,
            status,
            output_bytes,
        } => {
            let Some(call_id) = context.call_id() else {
                return Ok(());
            };
            transaction.execute(
                "INSERT INTO tool_calls (
                    call_id, session_id, branch_id, run_id, agent_id, turn_id,
                    attempt_id, finished_at, elapsed_ns, status, output_bytes
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(turn_id, call_id) DO UPDATE SET
                    finished_at = excluded.finished_at,
                    elapsed_ns = excluded.elapsed_ns,
                    status = excluded.status,
                    output_bytes = excluded.output_bytes",
                params![
                    id_text(call_id),
                    id_text(context.session_id()),
                    id_text(context.branch_id()),
                    id_text(context.run_id()),
                    id_text(context.agent_id()),
                    context.turn_id().map(id_text).unwrap_or_default(),
                    context.attempt_id().map(id_text),
                    timestamp,
                    duration_ns(elapsed),
                    terminal_status_text(status),
                    i64::try_from(output_bytes).unwrap_or(i64::MAX),
                ],
            )?;
        }
        TelemetryObservationKind::CompactionStarted { reason } => {
            // Pair Started→Finished on (session_id, branch_id, agent_id, turn_id-or-NULL):
            // update the most recent row with finished_at IS NULL; otherwise insert started-only.
            let turn_id = context.turn_id().map(id_text);
            transaction.execute(
                "INSERT INTO compactions (
                    session_id, branch_id, run_id, agent_id, turn_id,
                    started_at, reason
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    id_text(context.session_id()),
                    id_text(context.branch_id()),
                    id_text(context.run_id()),
                    id_text(context.agent_id()),
                    turn_id,
                    timestamp,
                    compaction_reason_text(reason),
                ],
            )?;
        }
        TelemetryObservationKind::CompactionFinished {
            refusal,
            elapsed,
            outcome,
            reason,
            tokens_before,
            tokens_after,
            usage,
        } => {
            let turn_id = context.turn_id().map(id_text);
            let (input, output, reasoning, cached_input, cache_write) = usage
                .map(usage_columns)
                .unwrap_or((None, None, None, None, None));
            let (cost_micros, cost_status) =
                usage.map_or((None, "unknown"), cost_fields_from_usage);
            let updated = transaction.execute(
                "UPDATE compactions SET
                    finished_at = ?1,
                    elapsed_ns = ?2,
                    outcome = ?3,
                    reason = ?4,
                    tokens_before = ?5,
                    tokens_after = ?6,
                    input_tokens = ?7,
                    output_tokens = ?8,
                    reasoning_tokens = ?9,
                    cached_input_tokens = ?10,
                    cache_write_tokens = ?11,
                    cost_micros = ?12,
                    cost_status = ?13,
                    refusal_reason = ?18
                 WHERE id = (
                    SELECT id FROM compactions
                    WHERE session_id = ?14 AND branch_id = ?15 AND agent_id = ?16
                      AND ((turn_id IS NULL AND ?17 IS NULL) OR turn_id = ?17)
                      AND finished_at IS NULL
                    ORDER BY id DESC
                    LIMIT 1
                 )",
                params![
                    timestamp,
                    duration_ns(elapsed),
                    compaction_outcome_text(outcome),
                    compaction_reason_text(reason),
                    tokens_before.map(i64::try_from).transpose().ok().flatten(),
                    tokens_after.map(i64::try_from).transpose().ok().flatten(),
                    input,
                    output,
                    reasoning,
                    cached_input,
                    cache_write,
                    cost_micros,
                    cost_status,
                    id_text(context.session_id()),
                    id_text(context.branch_id()),
                    id_text(context.agent_id()),
                    turn_id,
                    refusal.map(CompactionRefusalReason::as_str),
                ],
            )?;
            if updated == 0 {
                transaction.execute(
                    "INSERT INTO compactions (
                        session_id, branch_id, run_id, agent_id, turn_id,
                        finished_at, elapsed_ns, reason, outcome,
                        tokens_before, tokens_after,
                        input_tokens, output_tokens, reasoning_tokens,
                        cached_input_tokens, cache_write_tokens,
                        cost_micros, cost_status, refusal_reason
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
                    params![
                        id_text(context.session_id()),
                        id_text(context.branch_id()),
                        id_text(context.run_id()),
                        id_text(context.agent_id()),
                        turn_id,
                        timestamp,
                        duration_ns(elapsed),
                        compaction_reason_text(reason),
                        compaction_outcome_text(outcome),
                        tokens_before.map(i64::try_from).transpose().ok().flatten(),
                        tokens_after.map(i64::try_from).transpose().ok().flatten(),
                        input,
                        output,
                        reasoning,
                        cached_input,
                        cache_write,
                        cost_micros,
                        cost_status,
                        refusal.map(CompactionRefusalReason::as_str),
                    ],
                )?;
            }
        }
        TelemetryObservationKind::DurabilityFinished { elapsed, status } => {
            transaction.execute(
                "INSERT INTO durability_attempts (
                    session_id, branch_id, run_id, agent_id, turn_id,
                    at, elapsed_ns, kind, status
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    id_text(context.session_id()),
                    id_text(context.branch_id()),
                    id_text(context.run_id()),
                    id_text(context.agent_id()),
                    context.turn_id().map(id_text),
                    timestamp,
                    duration_ns(elapsed),
                    "durability_finished",
                    terminal_status_text(status),
                ],
            )?;
        }
        TelemetryObservationKind::DurabilityAttemptFinished {
            elapsed,
            attempt,
            outcome,
        } => {
            if attempt == DurabilityAttemptKind::InitialCommit
                && outcome == DurabilityAttemptOutcome::Committed
            {
                return Ok(());
            }
            transaction.execute(
                "INSERT INTO durability_attempts (
                    session_id, branch_id, run_id, agent_id, turn_id,
                    at, elapsed_ns, kind, attempt, outcome
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    id_text(context.session_id()),
                    id_text(context.branch_id()),
                    id_text(context.run_id()),
                    id_text(context.agent_id()),
                    context.turn_id().map(id_text),
                    timestamp,
                    duration_ns(elapsed),
                    "durability_attempt_finished",
                    durability_attempt_kind_text(attempt),
                    durability_attempt_outcome_text(outcome),
                ],
            )?;
        }
        TelemetryObservationKind::RetentionFinished {
            elapsed,
            status,
            pruned_checkpoints,
        } => {
            transaction.execute(
                "INSERT INTO retention_runs (
                    session_id, branch_id, run_id, agent_id, turn_id, at, elapsed_ns,
                    status, pruned_checkpoints
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    id_text(context.session_id()),
                    id_text(context.branch_id()),
                    id_text(context.run_id()),
                    id_text(context.agent_id()),
                    context.turn_id().map(id_text),
                    timestamp,
                    duration_ns(elapsed),
                    terminal_status_text(status),
                    i64::try_from(pruned_checkpoints).unwrap_or(i64::MAX),
                ],
            )?;
        }
        TelemetryObservationKind::ChildProcessExited {
            child,
            user_cpu,
            system_cpu,
            peak_rss_bytes,
        } => {
            transaction.execute(
                "INSERT INTO resource_samples (
                    at, session_id, agent_id, phase, child_kind, rss_bytes, peak_rss_bytes,
                    user_cpu_ns, system_cpu_ns, threads, fds
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    timestamp,
                    id_text(context.session_id()),
                    id_text(context.agent_id()),
                    "child_exited",
                    ChildKind::from_agent_child(child).as_str(),
                    None::<i64>,
                    peak_rss_bytes.map(i64::try_from).transpose().ok().flatten(),
                    duration_ns(user_cpu),
                    duration_ns(system_cpu),
                    None::<i64>,
                    None::<i64>,
                ],
            )?;
        }
        TelemetryObservationKind::PromptCacheDiagnosis {
            reason,
            missed_input_tokens,
        } => {
            transaction.execute(
                "INSERT INTO prompt_cache_diagnoses (
                    at, session_id, branch_id, run_id, agent_id, turn_id, attempt_id,
                    reason, missed_input_tokens
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    timestamp,
                    id_text(context.session_id()),
                    id_text(context.branch_id()),
                    id_text(context.run_id()),
                    id_text(context.agent_id()),
                    context.turn_id().map(id_text),
                    context.attempt_id().map(id_text),
                    reason.as_str(),
                    missed_input_tokens.map(|tokens| i64::try_from(tokens).unwrap_or(i64::MAX)),
                ],
            )?;
        }
        TelemetryObservationKind::CredentialRefresh {
            provider,
            phase,
            failure_class,
            transport_phase,
            http_status,
            elapsed,
            generation,
            detail,
        } => {
            transaction.execute(
                "INSERT INTO credential_refreshes (
                    at, provider, phase, failure_class, http_status, elapsed_ns, generation, transport_phase,
                    failure_reason
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    timestamp,
                    provider.as_str(),
                    credential_refresh_phase_text(phase),
                    failure_class.map(credential_refresh_failure_text),
                    http_status.map(i64::from),
                    elapsed.map(duration_ns),
                    i64::try_from(generation).unwrap_or(i64::MAX),
                    transport_phase.map(credential_transport_phase_text),
                    detail.reason.map(plakous::agent::CredentialRefreshFailureReason::as_str),
                ],
            )?;
        }
        _ => {}
    }
    Ok(())
}

type UsageColumns = (
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

fn usage_columns(usage: Usage) -> UsageColumns {
    (
        i64::try_from(usage.input_tokens()).ok(),
        i64::try_from(usage.output_tokens()).ok(),
        i64::try_from(usage.reasoning_tokens()).ok(),
        i64::try_from(usage.cached_input_tokens()).ok(),
        i64::try_from(usage.cache_write_tokens()).ok(),
    )
}

fn cost_fields_from_usage(usage: Usage) -> (Option<i64>, &'static str) {
    match usage.cost_micros() {
        Some(micros) => (Some(i64::try_from(micros).unwrap_or(i64::MAX)), "reported"),
        None => (None, "unknown"),
    }
}

fn id_text<T: std::fmt::Display>(id: T) -> String {
    id.to_string()
}

fn timestamp_ns(timestamp: Timestamp) -> i64 {
    let seconds = i64::try_from(timestamp.seconds()).unwrap_or(i64::MAX);
    seconds
        .saturating_mul(1_000_000_000)
        .saturating_add(i64::from(timestamp.nanoseconds()))
}

fn duration_ns(duration: Duration) -> i64 {
    i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX)
}

fn terminal_status_text(status: TelemetryTerminalStatus) -> &'static str {
    match status {
        TelemetryTerminalStatus::Completed => "completed",
        TelemetryTerminalStatus::Failed => "failed",
        TelemetryTerminalStatus::Cancelled => "cancelled",
        TelemetryTerminalStatus::Shutdown => "shutdown",
        _ => "unknown",
    }
}

fn durability_attempt_kind_text(kind: DurabilityAttemptKind) -> &'static str {
    match kind {
        DurabilityAttemptKind::InitialCommit => "initial_commit",
        DurabilityAttemptKind::InitialAcknowledgementRecovery => "initial_acknowledgement_recovery",
        DurabilityAttemptKind::CleanupToolDrain => "cleanup_tool_drain",
        DurabilityAttemptKind::CleanupJobTerminals => "cleanup_job_terminals",
        DurabilityAttemptKind::CleanupHeartbeatJoin => "cleanup_heartbeat_join",
        DurabilityAttemptKind::CleanupLeaseRelease => "cleanup_lease_release",
        DurabilityAttemptKind::CleanupFlush => "cleanup_flush",
        _ => "unknown",
    }
}

fn compaction_reason_text(reason: CompactionEventReason) -> &'static str {
    match reason {
        CompactionEventReason::Manual => "manual",
        CompactionEventReason::Scheduled => "scheduled",
    }
}

fn compaction_outcome_text(outcome: CompactionEventOutcome) -> &'static str {
    match outcome {
        CompactionEventOutcome::Committed => "committed",
        CompactionEventOutcome::Failed => "failed",
        CompactionEventOutcome::RecoveryRequired => "recovery_required",
        CompactionEventOutcome::NotNeeded => "not_needed",
        CompactionEventOutcome::NoEligibleRange => "no_eligible_range",
        CompactionEventOutcome::Unavailable => "unavailable",
    }
}

fn durability_attempt_outcome_text(outcome: DurabilityAttemptOutcome) -> &'static str {
    match outcome {
        DurabilityAttemptOutcome::Committed => "committed",
        DurabilityAttemptOutcome::Failed => "failed",
        DurabilityAttemptOutcome::RecoveredAcknowledgement => "recovered_acknowledgement",
        DurabilityAttemptOutcome::Joined => "joined",
        _ => "unknown",
    }
}

/// Shared phase label for the SQLite store and OTel export.
pub fn credential_refresh_phase_text(phase: CredentialRefreshPhase) -> &'static str {
    #[allow(
        unreachable_patterns,
        reason = "non-exhaustive telemetry enums reserve future variants"
    )]
    match phase {
        CredentialRefreshPhase::Began => "began",
        CredentialRefreshPhase::Completed => "completed",
        CredentialRefreshPhase::OutcomeUnknown => "outcome_unknown",
        CredentialRefreshPhase::Quarantined => "quarantined",
        CredentialRefreshPhase::Restored => "restored",
        _ => "unknown",
    }
}

/// Shared failure-class label for the SQLite store and OTel export.
pub fn credential_refresh_failure_text(class: CredentialRefreshFailureClass) -> &'static str {
    #[allow(
        unreachable_patterns,
        reason = "non-exhaustive telemetry enums reserve future variants"
    )]
    match class {
        CredentialRefreshFailureClass::Transport => "transport",
        CredentialRefreshFailureClass::Timeout => "timeout",
        CredentialRefreshFailureClass::HttpStatus(HttpStatusClass::ClientError) => "http_4xx",
        CredentialRefreshFailureClass::HttpStatus(HttpStatusClass::ServerError) => "http_5xx",
        CredentialRefreshFailureClass::HttpStatus(_) => "http_other",
        CredentialRefreshFailureClass::Validation => "validation",
        CredentialRefreshFailureClass::Store => "store",
        CredentialRefreshFailureClass::Cancelled => "cancelled",
        CredentialRefreshFailureClass::NotReady => "not_ready",
        _ => "unknown",
    }
}

/// Closed display-only transport phase label; contains no endpoint or credential.
pub(crate) fn credential_transport_phase_text(
    phase: plakous::model::ModelRetryPhase,
) -> &'static str {
    use plakous::model::ModelRetryPhase as P;
    match phase {
        P::Resolution => "resolution",
        P::Connect => "connect",
        P::Tls => "tls",
        P::RequestWrite | P::RequestWriteOrResponseHead => "send",
        P::ResponseHead | P::ResponseBody | P::Cleanup => "response",
    }
}

/// Latest credential-refresh observation for doctor (content-free).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialRefreshRow {
    pub provider: String,
    pub phase: String,
    pub failure_class: Option<String>,
    pub transport_phase: Option<String>,
    pub http_status_class: Option<String>,
    pub http_status: Option<u16>,
    pub generation: u64,
    pub at_ns: i64,
    pub failure_reason: Option<String>,
}

impl TelemetryStore {
    /// Read-only latest refresh row per provider for `plak doctor`.
    pub fn latest_credential_refresh(
        paths: &PlakPaths,
        provider: &str,
    ) -> Result<Option<CredentialRefreshRow>, TelemetryStoreOpenError> {
        Self::read_credential_refresh(paths, provider, false)
    }

    /// Latest settled failure; an in-flight Began does not hide it, and a
    /// completed refresh clears it. This read can never rebuild telemetry.
    pub(crate) fn latest_refresh_outcome(
        paths: &PlakPaths,
        provider: &str,
    ) -> Result<Option<CredentialRefreshRow>, TelemetryStoreOpenError> {
        Ok(Self::read_credential_refresh(paths, provider, true)?
            .filter(|row| row.phase != "completed"))
    }

    fn read_credential_refresh(
        paths: &PlakPaths,
        provider: &str,
        terminal_only: bool,
    ) -> Result<Option<CredentialRefreshRow>, TelemetryStoreOpenError> {
        let database = paths.telemetry_database();
        if !database.exists() {
            return Ok(None);
        }
        let connection = Connection::open_with_flags(
            database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|error| {
            TelemetryStoreOpenError::unavailable(format!(
                "could not inspect telemetry store: {error}"
            ))
        })?;
        TelemetryStore::require_attached_schema(&connection)?;
        let row = connection.query_row(
            "SELECT provider, phase, failure_class, http_status_class, http_status, generation, at, transport_phase,
                    failure_reason
                 FROM v_credential_refreshes
                 WHERE provider = ?1
                   AND (?2 = 0 OR phase IN ('completed', 'restored', 'outcome_unknown', 'quarantined'))
                 ORDER BY at DESC, id DESC LIMIT 1",
            params![provider, terminal_only],
            |row| {
                Ok(CredentialRefreshRow {
                    provider: row.get(0)?,
                    phase: row.get(1)?,
                    failure_class: row.get(2)?,
                    http_status_class: row.get(3)?,
                    http_status: row
                        .get::<_, Option<i64>>(4)?
                        .and_then(|value| u16::try_from(value).ok()),
                    generation: row.get::<_, i64>(5)?.try_into().unwrap_or(0),
                    at_ns: row.get(6)?,
                    transport_phase: row.get(7)?,
                    failure_reason: row.get(8)?,
                })
            },
        );
        match row {
            Ok(row) => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(TelemetryStoreOpenError::unavailable(error.to_string())),
        }
    }
}

impl TelemetryStore {
    /// Read-only time of the most recent automatic history prune that failed
    /// with no completed one after it, for `plak doctor`.
    pub fn unresolved_retention_failure(
        paths: &PlakPaths,
    ) -> Result<Option<i64>, TelemetryStoreOpenError> {
        let database = paths.telemetry_database();
        if !database.exists() {
            return Ok(None);
        }
        let connection = Connection::open_with_flags(
            database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|error| {
            TelemetryStoreOpenError::unavailable(format!(
                "could not inspect telemetry store: {error}"
            ))
        })?;
        TelemetryStore::require_attached_schema(&connection)?;
        connection
            .query_row(
                "SELECT max(at) FROM retention_runs WHERE status = 'failed'
                 AND at > COALESCE(
                     (SELECT max(at) FROM retention_runs WHERE status = 'completed'), -1)",
                [],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map_err(|error| TelemetryStoreOpenError::unavailable(error.to_string()))
    }
}

pub(crate) const WRITER_BATCH_DRAIN: usize = BATCH_DRAIN;

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::implicit_clone,
    clippy::used_underscore_binding,
    reason = "unit tests"
)]
mod projected_usage;

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::implicit_clone,
    clippy::used_underscore_binding,
    reason = "unit tests"
)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    use plakous::agent::{
        DurabilityAttemptKind, DurabilityAttemptOutcome, TelemetryContext, TelemetryObservation,
        TelemetryObservationKind, TelemetryTerminalStatus, TurnUsage,
    };
    use plakous::model::{
        AgentId, AttemptId, BranchId, CallId, RunId, SessionId, Timestamp, TurnId, Usage,
    };
    use uuid::Uuid;

    use crate::telemetry_sink::TelemetryWriter;
    use plakous_hosted::product_observation::ProductObservation;

    pub(super) type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    pub(super) fn fixture_context() -> TestResult<TelemetryContext> {
        Ok(TelemetryContext::new(
            AgentId::try_from(Uuid::from_u128(1))?,
            SessionId::try_from(Uuid::from_u128(2))?,
            BranchId::try_from(Uuid::from_u128(3))?,
            RunId::try_from(Uuid::from_u128(4))?,
        )
        .with_turn(TurnId::try_from(Uuid::from_u128(5))?)
        .with_attempt(AttemptId::try_from(Uuid::from_u128(6))?)
        .with_call(CallId::try_from(Uuid::from_u128(7))?))
    }

    pub(super) fn fixture_timestamp() -> TestResult<Timestamp> {
        Ok(Timestamp::new(8, 9)?)
    }

    #[test]
    fn clean_commit_has_one_row_but_failed_and_recovery_attempts_are_retained() -> TestResult {
        use plakous::agent::{DurabilityAttemptKind as K, DurabilityAttemptOutcome as O};
        let (_guard, paths) = PlakPaths::isolated_test_home();
        let mut store = TelemetryStore::open(&paths)?;
        let context = fixture_context()?;
        let at = fixture_timestamp()?;
        store.write_batch(&[
            TelemetryItem::Agent(TelemetryObservation::new(
                context,
                at,
                TelemetryObservationKind::DurabilityAttemptFinished {
                    elapsed: Duration::ZERO,
                    attempt: K::InitialCommit,
                    outcome: O::Committed,
                },
            )),
            TelemetryItem::Agent(TelemetryObservation::new(
                context,
                at,
                TelemetryObservationKind::DurabilityFinished {
                    elapsed: Duration::ZERO,
                    status: TelemetryTerminalStatus::Completed,
                },
            )),
        ])?;
        assert_eq!(
            store.query_i64("SELECT count(*) FROM durability_attempts"),
            Some(1)
        );
        for (attempt, outcome) in [
            (K::InitialCommit, O::Failed),
            (
                K::InitialAcknowledgementRecovery,
                O::RecoveredAcknowledgement,
            ),
        ] {
            store.write_batch(&[TelemetryItem::Agent(TelemetryObservation::new(
                context,
                at,
                TelemetryObservationKind::DurabilityAttemptFinished {
                    elapsed: Duration::ZERO,
                    attempt,
                    outcome,
                },
            ))])?;
        }
        assert_eq!(
            store.query_i64("SELECT count(*) FROM durability_attempts"),
            Some(3)
        );
        Ok(())
    }

    #[test]
    fn presented_tool_and_delta_fingerprint_are_recorded() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        let mut store = TelemetryStore::open(&paths)?;
        let context = fixture_context()?;
        let at = fixture_timestamp()?;
        let tool = plakous::model::ToolName::new("functions.exec")?;
        store.write_batch(&[
            TelemetryItem::Agent(TelemetryObservation::new(
                context,
                at,
                TelemetryObservationKind::ToolStarted {
                    tool: (&tool).into(),
                },
            )),
            TelemetryItem::Agent(TelemetryObservation::new(
                context,
                at,
                TelemetryObservationKind::ModelFinished {
                    elapsed: Duration::ZERO,
                    status: TelemetryTerminalStatus::Completed,
                    usage: Some(Usage::new(10, 20, 0, 0, 0)),
                    served_by: None,
                    output_deltas: 4,
                },
            )),
        ])?;
        let name: String =
            store
                .connection
                .query_row("SELECT tool_name FROM tool_calls", [], |row| row.get(0))?;
        let fingerprint: String = store.connection.query_row(
            "SELECT served_path_fingerprint FROM model_attempts",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(name, "functions.exec");
        assert_eq!(fingerprint, "dpt-v1:200");
        Ok(())
    }

    #[test]
    fn turn_origin_is_stored_for_user_prompts_and_host_wakes() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        let mut store = TelemetryStore::open(&paths)?;
        for origin in [
            plakous::model::TurnOrigin::UserPrompt,
            plakous::model::TurnOrigin::HostWake,
        ] {
            store.write_batch(&[TelemetryItem::Agent(TelemetryObservation::new(
                fixture_context()?,
                fixture_timestamp()?,
                TelemetryObservationKind::TurnStarted {
                    origin,
                    queue_time: Duration::ZERO,
                },
            ))])?;
            let stored: String =
                store
                    .connection
                    .query_row("SELECT origin FROM turns", [], |row| row.get(0))?;
            assert_eq!(stored, origin.as_str());
        }
        Ok(())
    }

    pub(super) fn all_observations() -> TestResult<Vec<TelemetryObservation>> {
        let context = fixture_context()?;
        let timestamp = fixture_timestamp()?;
        Ok(vec![
            TelemetryObservation::new(
                context,
                timestamp,
                TelemetryObservationKind::TurnStarted {
                    origin: plakous::model::TurnOrigin::UserPrompt,
                    queue_time: Duration::from_millis(10),
                },
            ),
            TelemetryObservation::new(
                context,
                timestamp,
                TelemetryObservationKind::TurnFinished {
                    elapsed: Duration::from_millis(20),
                    status: TelemetryTerminalStatus::Completed,
                    usage: TurnUsage::new(Usage::new(11, 12, 13, 14, 15), 1, 1, 2)?,
                    model_wall: Duration::from_millis(8),
                    tool_wall: Duration::from_millis(6),
                    compaction_wall: Duration::from_millis(2),
                    model_attempts: 1,
                    tool_calls: 1,
                    compactions: 0,
                },
            ),
            TelemetryObservation::new(context, timestamp, TelemetryObservationKind::ModelStarted),
            TelemetryObservation::new(
                context,
                timestamp,
                TelemetryObservationKind::ModelFirstEvent {
                    elapsed: Duration::from_millis(30),
                },
            ),
            TelemetryObservation::new(
                context,
                timestamp,
                TelemetryObservationKind::ModelFinished {
                    elapsed: Duration::from_millis(40),
                    status: TelemetryTerminalStatus::Completed,
                    usage: Some(Usage::new(21, 22, 23, 24, 25)),
                    served_by: None,
                    output_deltas: 0,
                },
            ),
            TelemetryObservation::new(
                context,
                timestamp,
                TelemetryObservationKind::ToolStarted {
                    tool: (&plakous::model::ToolName::new("test").unwrap()).into(),
                },
            ),
            TelemetryObservation::new(
                context,
                timestamp,
                TelemetryObservationKind::ToolFinished {
                    elapsed: Duration::from_millis(50),
                    status: TelemetryTerminalStatus::Completed,
                    output_bytes: 128,
                },
            ),
            TelemetryObservation::new(
                context,
                timestamp,
                TelemetryObservationKind::CompactionStarted {
                    reason: CompactionEventReason::Manual,
                },
            ),
            TelemetryObservation::new(
                context,
                timestamp,
                TelemetryObservationKind::CompactionFinished {
                    refusal: None,
                    elapsed: Duration::from_millis(55),
                    outcome: CompactionEventOutcome::Committed,
                    reason: CompactionEventReason::Manual,
                    tokens_before: Some(100),
                    tokens_after: Some(40),
                    usage: Some(Usage::new(1, 2, 3, 4, 5)),
                },
            ),
            TelemetryObservation::new(
                context,
                timestamp,
                TelemetryObservationKind::DurabilityFinished {
                    elapsed: Duration::from_millis(60),
                    status: TelemetryTerminalStatus::Completed,
                },
            ),
            TelemetryObservation::new(
                context,
                timestamp,
                TelemetryObservationKind::RetentionFinished {
                    elapsed: Duration::from_millis(7),
                    status: TelemetryTerminalStatus::Failed,
                    pruned_checkpoints: 0,
                },
            ),
            TelemetryObservation::new(
                context,
                timestamp,
                TelemetryObservationKind::ChildProcessExited {
                    child: plakous::agent::ChildProcessKind::JsEvalSidecar,
                    user_cpu: Duration::from_millis(2),
                    system_cpu: Duration::from_millis(1),
                    peak_rss_bytes: Some(4096),
                },
            ),
            TelemetryObservation::new(
                context,
                timestamp,
                TelemetryObservationKind::CredentialRefresh {
                    provider: plakous::agent::CredentialProviderLabel::new("xai").unwrap(),
                    phase: CredentialRefreshPhase::OutcomeUnknown,
                    failure_class: Some(CredentialRefreshFailureClass::Timeout),
                    transport_phase: Some(plakous::model::ModelRetryPhase::Resolution),
                    http_status: Some(503),
                    elapsed: Some(Duration::from_millis(30)),
                    generation: 40,
                    detail: plakous::agent::CredentialRefreshDetail::default(),
                },
            ),
            TelemetryObservation::new(
                context,
                timestamp,
                TelemetryObservationKind::PromptCacheDiagnosis {
                    reason: plakous::model::PromptCacheMissReason::MessagesChanged,
                    missed_input_tokens: Some(587),
                },
            ),
        ])
    }

    #[test]
    fn durability_variants_map_to_stable_snake_case() {
        assert_eq!(
            durability_attempt_kind_text(DurabilityAttemptKind::InitialCommit),
            "initial_commit"
        );
        assert_eq!(
            durability_attempt_kind_text(DurabilityAttemptKind::InitialAcknowledgementRecovery),
            "initial_acknowledgement_recovery"
        );
        assert_eq!(
            durability_attempt_outcome_text(DurabilityAttemptOutcome::Committed),
            "committed"
        );
        assert_eq!(
            durability_attempt_outcome_text(DurabilityAttemptOutcome::Failed),
            "failed"
        );
        assert_eq!(
            durability_attempt_outcome_text(DurabilityAttemptOutcome::RecoveredAcknowledgement),
            "recovered_acknowledgement"
        );
    }

    #[test]
    fn turn_finished_cannot_write_projected_usage() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        let mut store = TelemetryStore::open(&paths)?;
        let context = fixture_context()?;
        store.write_batch(&[TelemetryItem::Agent(TelemetryObservation::new(
            context,
            fixture_timestamp()?,
            TelemetryObservationKind::TurnFinished {
                elapsed: Duration::from_millis(20),
                status: TelemetryTerminalStatus::Completed,
                usage: TurnUsage::new(
                    Usage::new(1, 2, 0, 0, 0).with_cost_micros(Some(42_000)),
                    1,
                    1,
                    0,
                )?,
                model_wall: Duration::ZERO,
                tool_wall: Duration::ZERO,
                compaction_wall: Duration::ZERO,
                model_attempts: 1,
                tool_calls: 0,
                compactions: 0,
            },
        ))])?;
        assert_eq!(store.query_i64("SELECT cost_micros FROM turns"), None);
        assert_eq!(store.query_i64("SELECT input_tokens FROM turns"), None);
        Ok(())
    }

    #[test]
    fn same_call_id_in_two_turns_keeps_two_tool_rows() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        let mut store = TelemetryStore::open(&paths)?;
        let first = fixture_context()?;
        let second = first.with_turn(TurnId::try_from(Uuid::from_u128(55))?);
        let finished = |context| {
            TelemetryItem::Agent(TelemetryObservation::new(
                context,
                fixture_timestamp().expect("fixture timestamp"),
                TelemetryObservationKind::ToolFinished {
                    elapsed: Duration::from_millis(5),
                    status: TelemetryTerminalStatus::Completed,
                    output_bytes: 1,
                },
            ))
        };
        let started = |context| {
            TelemetryItem::Agent(TelemetryObservation::new(
                context,
                fixture_timestamp().expect("fixture timestamp"),
                TelemetryObservationKind::ToolStarted {
                    tool: (&plakous::model::ToolName::new("test").unwrap()).into(),
                },
            ))
        };
        store.write_batch(&[
            started(first),
            finished(first),
            started(second),
            finished(second),
        ])?;
        assert_eq!(store.query_i64("SELECT count(*) FROM tool_calls"), Some(2));
        assert_eq!(
            store.query_i64("SELECT count(DISTINCT turn_id) FROM tool_calls"),
            Some(2)
        );
        Ok(())
    }

    #[test]
    fn every_observation_kind_lands_as_one_row() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        let store = TelemetryStore::open(&paths)?;
        let writer = TelemetryWriter::spawn(store, 4096)?;
        let sink: Arc<dyn plakous::agent::TelemetrySink> =
            Arc::clone(&writer) as Arc<dyn plakous::agent::TelemetrySink>;
        let emitter = Arc::new(plakous::agent::TelemetryEmitter::enabled(sink));
        let context = fixture_context()?;
        writer.emit_product(ProductObservation::TurnUsage {
            at: fixture_timestamp()?,
            session_id: context.session_id(),
            branch_id: context.branch_id(),
            turn_id: context.turn_id().unwrap(),
            usage: TurnUsage::new(Usage::new(11, 12, 13, 14, 15), 1, 1, 2)?,
        });
        for observation in all_observations()? {
            emitter.emit(observation);
        }
        let status = writer.close_and_join()?;
        assert_eq!(status.write_errors(), 0);

        let store = TelemetryStore::open(&paths)?;
        assert_eq!(store.count_rows("turns"), 1);
        assert_eq!(store.count_rows("model_attempts"), 1);
        assert_eq!(store.count_rows("tool_calls"), 1);
        assert_eq!(store.count_rows("durability_attempts"), 1);
        assert_eq!(store.count_rows("retention_runs"), 1);
        assert_eq!(
            TelemetryStore::unresolved_retention_failure(&paths)?,
            store.query_i64("SELECT at FROM retention_runs")
        );

        assert_eq!(store.count_rows("compactions"), 1);
        assert_eq!(store.count_rows("resource_samples"), 4);
        assert_eq!(store.count_rows("prompt_cache_diagnoses"), 1);
        assert_eq!(
            store.query_text("SELECT reason FROM prompt_cache_diagnoses"),
            Some("messages_changed".to_owned())
        );
        assert_eq!(
            store.query_i64("SELECT missed_input_tokens FROM prompt_cache_diagnoses"),
            Some(587)
        );

        assert_eq!(
            store.query_i64("SELECT model_wall_ns FROM turns").unwrap(),
            duration_ns(Duration::from_millis(8))
        );
        assert_eq!(
            store
                .query_i64("SELECT output_bytes FROM tool_calls")
                .unwrap(),
            128
        );
        assert_eq!(
            store
                .query_i64("SELECT ttft_ns FROM model_attempts")
                .unwrap(),
            duration_ns(Duration::from_millis(30))
        );
        assert_eq!(
            store
                .query_i64("SELECT ttlb_ns FROM model_attempts")
                .unwrap(),
            duration_ns(Duration::from_millis(40))
        );
        assert_eq!(
            store.query_i64("SELECT cache_write_tokens FROM model_attempts"),
            Some(25)
        );
        assert_eq!(
            store.query_i64("SELECT cache_write_tokens FROM turns"),
            Some(15)
        );
        assert_eq!(
            store.query_text("SELECT session_id FROM turns").unwrap(),
            SessionId::try_from(Uuid::from_u128(2))?.to_string()
        );
        Ok(())
    }

    #[test]
    fn version_mismatch_refuses_without_deleting_rows() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        {
            let store = TelemetryStore::open(&paths)?;
            let writer = TelemetryWriter::spawn(store, 16)?;
            let sink: Arc<dyn plakous::agent::TelemetrySink> =
                Arc::clone(&writer) as Arc<dyn plakous::agent::TelemetrySink>;
            let emitter = Arc::new(plakous::agent::TelemetryEmitter::enabled(sink));
            emitter.emit(all_observations()?[0]);
            writer.close_and_join()?;
        }
        {
            let connection = Connection::open(paths.telemetry_database())?;
            connection.pragma_update(None, "user_version", 99)?;
        }
        assert!(TelemetryStore::open(&paths).is_err());
        let connection = Connection::open(paths.telemetry_database())?;
        let rows: i64 = connection.query_row("SELECT COUNT(*) FROM turns", [], |row| row.get(0))?;
        assert_eq!(rows, 1);
        Ok(())
    }

    #[test]
    fn compaction_finished_without_start_inserts_finished_only_row() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        let mut store = TelemetryStore::open(&paths)?;
        let context = fixture_context()?;
        store.write_batch(&[TelemetryItem::Agent(TelemetryObservation::new(
            context,
            fixture_timestamp()?,
            TelemetryObservationKind::CompactionFinished {
                refusal: None,
                elapsed: Duration::from_millis(12),
                outcome: CompactionEventOutcome::NotNeeded,
                reason: CompactionEventReason::Scheduled,
                tokens_before: None,
                tokens_after: None,
                usage: None,
            },
        ))])?;
        assert_eq!(store.count_rows("compactions"), 1);
        assert_eq!(
            store.query_text("SELECT outcome FROM compactions").unwrap(),
            "not_needed"
        );
        assert!(
            store
                .query_i64("SELECT started_at FROM compactions")
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn compaction_cost_tracks_reported_completed_and_unknown_responses() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        let mut store = TelemetryStore::open(&paths)?;
        let context = fixture_context()?;
        let at = fixture_timestamp()?;
        for (outcome, usage) in [
            (
                CompactionEventOutcome::Committed,
                Some(Usage::new(11, 2, 1, 3, 4).with_cost_micros(Some(125))),
            ),
            (
                CompactionEventOutcome::Failed,
                Some(Usage::new(21, 5, 4, 6, 0).with_cost_micros(Some(666))),
            ),
            (
                CompactionEventOutcome::Committed,
                Some(Usage::new(31, 8, 0, 1, 0)),
            ),
            (CompactionEventOutcome::Failed, None),
        ] {
            store.write_batch(&[
                TelemetryItem::Agent(TelemetryObservation::new(
                    context,
                    at,
                    TelemetryObservationKind::CompactionStarted {
                        reason: CompactionEventReason::Scheduled,
                    },
                )),
                TelemetryItem::Agent(TelemetryObservation::new(
                    context,
                    at,
                    TelemetryObservationKind::CompactionFinished {
                        refusal: None,
                        elapsed: Duration::from_millis(1),
                        outcome,
                        reason: CompactionEventReason::Scheduled,
                        tokens_before: None,
                        tokens_after: None,
                        usage,
                    },
                )),
            ])?;
        }
        let mut rows = store.connection.prepare(
            "SELECT outcome, input_tokens, cost_micros, cost_status FROM compactions ORDER BY id",
        )?;
        let actual = rows
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(
            actual,
            vec![
                (
                    "committed".to_owned(),
                    Some(11),
                    Some(125),
                    "reported".to_owned()
                ),
                (
                    "failed".to_owned(),
                    Some(21),
                    Some(666),
                    "reported".to_owned()
                ),
                ("committed".to_owned(), Some(31), None, "unknown".to_owned()),
                ("failed".to_owned(), None, None, "unknown".to_owned()),
            ]
        );
        Ok(())
    }

    #[test]
    fn product_observations_land_in_product_events() -> TestResult {
        use plakous_hosted::product_observation::{
            BoundedText64, JsonlCommandKind, ProductObservation, ResourceSamplePhase,
            session_id_from_text, wall_timestamp,
        };

        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        let mut store = TelemetryStore::open(&paths)?;
        let session_id = SessionId::try_from(Uuid::from_u128(2))?;
        let tree_id = BoundedText64::new("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap();
        let node_id = BoundedText64::new("root").unwrap();
        store.write_batch(&[
            TelemetryItem::Product(ProductObservation::TreeNodeSpawned {
                at: wall_timestamp(),
                session_id,
                tree_id,
                node_id,
                parent_node_id: None,
                template: BoundedText64::new("review").unwrap(),
            }),
            TelemetryItem::Product(ProductObservation::JsonlCommand {
                at: wall_timestamp(),
                session_id,
                kind: JsonlCommandKind::Prompt,
                admitted_to_completed: Duration::from_millis(4),
            }),
            TelemetryItem::Product(ProductObservation::SessionOpened {
                at: wall_timestamp(),
                session_id,
                elapsed: Duration::from_millis(5),
            }),
            TelemetryItem::Product(ProductObservation::ResourceSample {
                at: wall_timestamp(),
                session_id,
                agent_id: Some(AgentId::try_from(Uuid::from_u128(1))?),
                phase: ResourceSamplePhase::TurnStarted,
                child_kind: None,
                rss_bytes: Some(1024),
                peak_rss_bytes: Some(2048),
                user_cpu: Duration::from_millis(1),
                system_cpu: Duration::from_millis(1),
                threads: Some(2),
                fds: Some(3),
            }),
        ])?;
        assert_eq!(store.count_rows("product_events"), 3);
        assert_eq!(store.count_rows("resource_samples"), 1);
        assert!(session_id_from_text("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").is_some());
        Ok(())
    }

    #[test]
    fn semantic_activity_and_declined_retry_columns_are_available() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        let store = TelemetryStore::open(&paths)?;
        assert_eq!(store.query_i64("SELECT COUNT(*) FROM pragma_table_info('model_attempts') WHERE name = 'keepalive_count'"), Some(1));
        assert_eq!(store.query_i64("SELECT COUNT(*) FROM pragma_table_info('model_retries') WHERE name = 'declined_reason'"), Some(1));
        Ok(())
    }

    #[test]
    fn semantic_activity_rows_retain_counts_and_the_last_semantic_time() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        let mut store = TelemetryStore::open(&paths)?;
        store.write_batch(&[TelemetryItem::Agent(TelemetryObservation::new(
            fixture_context()?,
            Timestamp::new(500, 0)?,
            TelemetryObservationKind::ModelStreamActivity {
                activity: plakous::model::ModelStreamActivity {
                    output_started: true,
                    last_semantic_event_elapsed_ms: Some(100),
                    semantic_gap_ms: 300_000,
                    keepalive_count: 7,
                    max_semantic_gap_ms: 300_000,
                },
            },
        ))])?;
        assert_eq!(
            store.query_i64("SELECT last_semantic_event_at FROM model_attempts"),
            Some(200_000_000_000)
        );
        assert_eq!(
            store.query_i64("SELECT keepalive_count FROM model_attempts"),
            Some(7)
        );
        assert_eq!(
            store.query_i64("SELECT max_semantic_gap_ms FROM model_attempts"),
            Some(300_000)
        );
        Ok(())
    }

    #[test]
    fn replay_loss_rows_are_content_free_and_pruned_with_their_session() -> TestResult {
        use plakous::model::{ModelRetryCause, ModelRetryLoss, ModelRetryPhase};
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        let mut store = TelemetryStore::open(&paths)?;
        let session_id = SessionId::try_from(Uuid::from_u128(31))?;
        for next_attempt in [2, 3] {
            store.write_batch(&[TelemetryItem::Product(ProductObservation::ModelRetry {
                at: Timestamp::new(1, 0)?,
                session_id,
                branch_id: BranchId::try_from(Uuid::from_u128(32))?,
                agent_id: AgentId::try_from(Uuid::from_u128(33))?,
                turn_id: TurnId::try_from(Uuid::from_u128(34))?,
                next_attempt,
                max_attempts: 4,
                cause: if next_attempt == 2 {
                    ModelRetryCause::RecordRejectedInTransit
                } else {
                    ModelRetryCause::StreamInterrupted {
                        fault: plakous::model::ModelTransportFault::EndOfStream,
                    }
                },
                loss: ModelRetryLoss {
                    assistant_text_bytes: 7,
                    reasoning_bytes: 11,
                    completed_items: 2,
                    elapsed_ms: 17,
                    server_effect: false,
                    transport_phase: Some(if next_attempt == 2 {
                        ModelRetryPhase::RequestWriteOrResponseHead
                    } else {
                        ModelRetryPhase::ResponseBody
                    }),
                    declined_reason: (next_attempt == 3)
                        .then_some(plakous::model::ModelRetryDeclineReason::Budget),
                },
            })])?;
        }
        assert_eq!(
            store.query_i64("SELECT COUNT(*) FROM v_replay_loss"),
            Some(2)
        );
        assert_eq!(store.query_i64("PRAGMA user_version"), Some(19));
        assert_eq!(
            store.query_i64(
                "SELECT COUNT(*) FROM model_retries WHERE cause_kind = 'record_rejected_in_transit'"
            ),
            Some(1)
        );
        assert_eq!(
            store.query_i64("SELECT COUNT(*) FROM model_retries WHERE declined_reason = 'budget'"),
            Some(1)
        );

        assert_eq!(
            store
                .query_i64("SELECT SUM(assistant_text_bytes + reasoning_bytes) FROM v_replay_loss"),
            Some(36)
        );
        assert_eq!(
            store
                .query_text("SELECT replay_safety FROM v_replay_loss LIMIT 1")
                .as_deref(),
            Some("cost_only")
        );
        assert_eq!(
            store
                .query_text("SELECT transport_phase FROM v_replay_loss WHERE cause_kind = 'record_rejected_in_transit'")
                .as_deref(),
            Some("request_write_or_response_head")
        );
        delete_session_rows(&store.connection, &id_text(session_id))?;
        assert_eq!(store.count_rows("model_retries"), 0);
        Ok(())
    }

    #[test]
    fn tree_node_detached_status_lands_in_product_events() -> TestResult {
        use plakous_hosted::product_observation::{
            BoundedText64, NodeDetachStatus, ProductObservation, wall_timestamp,
        };

        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        let mut store = TelemetryStore::open(&paths)?;
        let session_id = SessionId::try_from(Uuid::from_u128(3))?;
        let tree_id = BoundedText64::new("bbbbbbbb-cccc-dddd-eeee-ffffffffffff").unwrap();
        let node_id = BoundedText64::new("c1").unwrap();
        store.write_batch(&[TelemetryItem::Product(
            ProductObservation::TreeNodeDetached {
                at: wall_timestamp(),
                session_id,
                tree_id,
                node_id,
                parent_node_id: BoundedText64::new("root"),
                status: NodeDetachStatus::Closed,
            },
        )])?;
        assert_eq!(store.count_rows("product_events"), 1);
        assert_eq!(
            store
                .query_text("SELECT status FROM product_events WHERE kind = 'tree_node_detached'")
                .unwrap(),
            "closed"
        );
        assert!(
            store
                .query_text(
                    "SELECT spawn_class FROM product_events WHERE kind = 'tree_node_detached'"
                )
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn resource_sample_has_nonzero_cpu_and_peak_rss() -> TestResult {
        use plakous_hosted::product_observation::{
            ProductObservation, ResourceSamplePhase, wall_timestamp,
        };
        use plakous_hosted::runtime::sample_process_resources;

        let snapshot = sample_process_resources();
        assert!(
            snapshot.user_cpu > Duration::ZERO || snapshot.system_cpu > Duration::ZERO,
            "expected non-zero CPU sample on this OS"
        );
        assert!(
            snapshot.peak_rss_bytes.unwrap_or(0) > 0,
            "expected peak RSS on this OS"
        );

        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        let mut store = TelemetryStore::open(&paths)?;
        let session_id = SessionId::try_from(Uuid::from_u128(2))?;
        store.write_batch(
            &[TelemetryItem::Product(ProductObservation::ResourceSample {
                at: wall_timestamp(),
                session_id,
                agent_id: None,
                phase: ResourceSamplePhase::TurnFinished,
                child_kind: None,
                rss_bytes: snapshot.rss_bytes,
                peak_rss_bytes: snapshot.peak_rss_bytes,
                user_cpu: snapshot.user_cpu,
                system_cpu: snapshot.system_cpu,
                threads: snapshot.threads,
                fds: snapshot.fds,
            })],
        )?;
        let user_cpu = store
            .query_i64("SELECT user_cpu_ns FROM resource_samples")
            .unwrap();
        let system_cpu = store
            .query_i64("SELECT system_cpu_ns FROM resource_samples")
            .unwrap();
        let peak = store
            .query_i64("SELECT peak_rss_bytes FROM resource_samples")
            .unwrap_or(0);
        assert!((user_cpu + system_cpu) > 0);
        assert!(peak > 0);
        Ok(())
    }

    #[test]
    fn child_exited_sample_persists_child_kind() -> TestResult {
        use plakous::agent::{
            ChildProcessKind, TelemetryContext, TelemetryObservation, TelemetryObservationKind,
        };
        use plakous::model::{AgentId, BranchId, RunId, SessionId, Timestamp};

        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        let mut store = TelemetryStore::open(&paths)?;
        let session_id = SessionId::try_from(Uuid::from_u128(42))?;
        let agent_id = AgentId::try_from(Uuid::from_u128(43))?;
        let branch_id = BranchId::try_from(Uuid::from_u128(44))?;
        let run_id = RunId::try_from(Uuid::from_u128(45))?;
        let context = TelemetryContext::new(agent_id, session_id, branch_id, run_id);
        store.write_batch(&[TelemetryItem::Agent(TelemetryObservation::new(
            context,
            Timestamp::new(100, 0)?,
            TelemetryObservationKind::ChildProcessExited {
                child: ChildProcessKind::JsEvalSidecar,
                user_cpu: Duration::from_millis(3),
                system_cpu: Duration::from_millis(1),
                peak_rss_bytes: Some(8192),
            },
        ))])?;
        assert_eq!(store.count_rows("resource_samples"), 1);
        assert_eq!(
            store
                .query_text("SELECT phase FROM resource_samples")
                .unwrap(),
            "child_exited"
        );
        assert_eq!(
            store
                .query_text("SELECT child_kind FROM resource_samples")
                .unwrap(),
            "js_eval_sidecar"
        );
        Ok(())
    }

    #[test]
    fn prune_by_age_removes_whole_old_sessions() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        let old_session = SessionId::try_from(Uuid::from_u128(10))?;
        let new_session = SessionId::try_from(Uuid::from_u128(11))?;
        let mut store = TelemetryStore::open(&paths)?;
        store.write_batch(&[
            TelemetryItem::Product(ProductObservation::SessionOpened {
                at: Timestamp::new(1, 0)?,
                session_id: old_session,
                elapsed: Duration::from_millis(1),
            }),
            TelemetryItem::Product(ProductObservation::SessionOpened {
                at: Timestamp::new(9_999_999_999, 0)?,
                session_id: new_session,
                elapsed: Duration::from_millis(1),
            }),
        ])?;
        store.prune(
            TelemetryRetentionConfig {
                max_bytes: u64::MAX,
                max_age_days: 1,
            },
            &paths.telemetry_database(),
        )?;
        assert_eq!(store.count_rows("product_events"), 1);
        assert_eq!(
            store
                .query_text("SELECT session_id FROM product_events")
                .unwrap(),
            new_session.to_string()
        );
        Ok(())
    }

    fn session_context(session: SessionId) -> TestResult<TelemetryContext> {
        Ok(TelemetryContext::new(
            AgentId::try_from(Uuid::from_u128(1))?,
            session,
            BranchId::try_from(Uuid::from_u128(3))?,
            RunId::try_from(Uuid::from_u128(4))?,
        ))
    }

    fn cache_diagnosis_item(session: SessionId, at: Timestamp) -> TestResult<TelemetryItem> {
        Ok(TelemetryItem::Agent(TelemetryObservation::new(
            session_context(session)?,
            at,
            TelemetryObservationKind::PromptCacheDiagnosis {
                reason: plakous::model::PromptCacheMissReason::MessagesChanged,
                missed_input_tokens: Some(4),
            },
        )))
    }

    fn job_event_item(session: SessionId, at: Timestamp) -> TestResult<TelemetryItem> {
        use plakous::agent::{JobHintSource, JobTool};
        Ok(TelemetryItem::Agent(TelemetryObservation::new(
            session_context(session)?,
            at,
            TelemetryObservationKind::JobPromoted {
                tool: JobTool::Start,
                nested: false,
                hint_source: JobHintSource::Default,
                hint_ms: 1,
            },
        )))
    }

    #[test]
    fn prompt_cache_diagnoses_are_counted_and_nullable() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        let mut store = TelemetryStore::open(&paths)?;
        let columns = {
            let mut statement = store.connection.prepare(
                "SELECT name, \"notnull\" FROM pragma_table_info('prompt_cache_diagnoses') ORDER BY cid",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        assert_eq!(
            columns,
            [
                ("id", 1),
                ("at", 1),
                ("session_id", 1),
                ("branch_id", 1),
                ("run_id", 1),
                ("agent_id", 1),
                ("turn_id", 0),
                ("attempt_id", 0),
                ("reason", 1),
                ("missed_input_tokens", 0),
            ]
            .map(|(name, required)| (name.to_owned(), required))
        );
        store.write_batch(&[TelemetryItem::Agent(TelemetryObservation::new(
            fixture_context()?,
            fixture_timestamp()?,
            TelemetryObservationKind::PromptCacheDiagnosis {
                reason: plakous::model::PromptCacheMissReason::Unavailable,
                missed_input_tokens: None,
            },
        ))])?;
        assert_eq!(
            store.query_i64("SELECT missed_input_tokens IS NULL FROM prompt_cache_diagnoses"),
            Some(1)
        );
        let status = store.status_snapshot()?;
        assert!(
            status
                .table_counts
                .iter()
                .any(|(name, count)| { name == "prompt_cache_diagnoses" && *count == 1 })
        );
        assert_eq!(TelemetryStore::count_all_rows(&paths)?, 1);
        Ok(())
    }

    #[test]
    fn prune_by_age_removes_prompt_cache_diagnoses_and_job_events() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        let old_session = SessionId::try_from(Uuid::from_u128(30))?;
        let new_session = SessionId::try_from(Uuid::from_u128(31))?;
        let old_at = Timestamp::new(1, 0)?;
        let new_at = Timestamp::new(9_999_999_999, 0)?;
        let mut store = TelemetryStore::open(&paths)?;
        store.write_batch(&[
            cache_diagnosis_item(old_session, old_at)?,
            job_event_item(old_session, old_at)?,
            cache_diagnosis_item(new_session, new_at)?,
            job_event_item(new_session, new_at)?,
        ])?;
        store.prune(
            TelemetryRetentionConfig {
                max_bytes: u64::MAX,
                max_age_days: 1,
            },
            &paths.telemetry_database(),
        )?;
        assert_eq!(store.count_rows("prompt_cache_diagnoses"), 1);
        assert_eq!(store.count_rows("job_events"), 1);
        assert_eq!(
            store.query_text("SELECT session_id FROM prompt_cache_diagnoses"),
            Some(new_session.to_string())
        );
        assert_eq!(
            store.query_text("SELECT session_id FROM job_events"),
            Some(new_session.to_string())
        );
        Ok(())
    }

    #[test]
    fn prune_by_bytes_removes_prompt_cache_diagnoses_and_job_events() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        let old_session = SessionId::try_from(Uuid::from_u128(32))?;
        let new_session = SessionId::try_from(Uuid::from_u128(33))?;
        let mut store = TelemetryStore::open(&paths)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| error.to_string())?;
        let old_at = Timestamp::new(now.as_secs().saturating_sub(3_600), 0)?;
        let new_at = Timestamp::new(now.as_secs(), 0)?;
        fill_session(&mut store, old_session, old_at, 400)?;
        fill_session(&mut store, new_session, new_at, 400)?;
        store.write_batch(&[
            cache_diagnosis_item(old_session, old_at)?,
            job_event_item(old_session, old_at)?,
            cache_diagnosis_item(new_session, new_at)?,
            job_event_item(new_session, new_at)?,
        ])?;
        assert_eq!(store.checkpoint_wal()?, WalCheckpointOutcome::Completed);
        let database = paths.telemetry_database();
        let size_both = TelemetryStore::database_size_bytes(&database);
        let outcome = store.prune(
            TelemetryRetentionConfig {
                max_bytes: size_both.saturating_sub(1),
                max_age_days: 90,
            },
            &database,
        )?;
        assert_eq!(outcome.wal_checkpoint, WalCheckpointOutcome::Completed);
        assert_eq!(store.count_rows("prompt_cache_diagnoses"), 1);
        assert_eq!(store.count_rows("job_events"), 1);
        assert_eq!(
            store.query_text("SELECT session_id FROM prompt_cache_diagnoses"),
            Some(new_session.to_string())
        );
        assert_eq!(
            store.query_text("SELECT session_id FROM job_events"),
            Some(new_session.to_string())
        );
        Ok(())
    }

    fn recent_timestamp() -> TestResult<Timestamp> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| error.to_string())?;
        Ok(Timestamp::new(now.as_secs(), 0)?)
    }

    fn session_opened(session: SessionId, at: Timestamp) -> TelemetryItem {
        TelemetryItem::Product(ProductObservation::SessionOpened {
            at,
            session_id: session,
            elapsed: Duration::from_millis(1),
        })
    }

    fn wal_file_len(database: &Path) -> u64 {
        sqlite_file_len(&sqlite_sidecar(database, "-wal"))
    }

    fn fill_session(
        store: &mut TelemetryStore,
        session: SessionId,
        at: Timestamp,
        rows: usize,
    ) -> TestResult {
        for _ in 0..rows {
            store.write_batch(&[session_opened(session, at)])?;
        }
        Ok(())
    }

    #[test]
    fn telemetry_wal_growth_counts_toward_the_byte_cap() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        let mut store = TelemetryStore::open(&paths)?;
        let session = SessionId::try_from(Uuid::from_u128(21))?;
        fill_session(&mut store, session, recent_timestamp()?, 128)?;
        let database = paths.telemetry_database();
        let main = sqlite_file_len(&database);
        let wal = wal_file_len(&database);
        let shm = sqlite_file_len(&sqlite_sidecar(&database, "-shm"));
        let total = TelemetryStore::database_size_bytes(&database);
        assert!(
            wal > 0,
            "expected WAL growth after uncheckpointed writes, wal={wal}"
        );
        assert_eq!(total, main.saturating_add(wal).saturating_add(shm));
        assert!(
            total > main,
            "byte cap must count WAL/SHM, main={main} total={total}"
        );
        Ok(())
    }

    #[test]
    fn telemetry_prune_checkpoint_truncates_wal() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        let mut store = TelemetryStore::open(&paths)?;
        let session = SessionId::try_from(Uuid::from_u128(22))?;
        fill_session(&mut store, session, recent_timestamp()?, 128)?;
        let database = paths.telemetry_database();
        let wal_before = wal_file_len(&database);
        assert!(wal_before > 0, "expected a WAL file before prune");
        let outcome = store.prune(
            TelemetryRetentionConfig {
                max_bytes: u64::MAX,
                max_age_days: 90,
            },
            &database,
        )?;
        assert_eq!(outcome.wal_checkpoint, WalCheckpointOutcome::Completed);
        let wal_after = wal_file_len(&database);
        assert!(
            wal_after < wal_before,
            "TRUNCATE should shrink the WAL, before={wal_before} after={wal_after}"
        );
        assert_eq!(store.count_rows("product_events"), 128);
        Ok(())
    }

    #[test]
    fn telemetry_byte_cap_is_met_without_over_deleting() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        let old_session = SessionId::try_from(Uuid::from_u128(23))?;
        let new_session = SessionId::try_from(Uuid::from_u128(24))?;
        let mut store = TelemetryStore::open(&paths)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| error.to_string())?;
        let old_at = Timestamp::new(now.as_secs().saturating_sub(3_600), 0)?;
        let new_at = Timestamp::new(now.as_secs(), 0)?;
        fill_session(&mut store, old_session, old_at, 400)?;
        fill_session(&mut store, new_session, new_at, 400)?;
        assert_eq!(store.checkpoint_wal()?, WalCheckpointOutcome::Completed);
        let database = paths.telemetry_database();
        let size_both = TelemetryStore::database_size_bytes(&database);
        assert!(size_both > 1, "expected allocated pages for two sessions");
        let outcome = store.prune(
            TelemetryRetentionConfig {
                max_bytes: size_both.saturating_sub(1),
                max_age_days: 90,
            },
            &database,
        )?;
        assert_eq!(outcome.wal_checkpoint, WalCheckpointOutcome::Completed);
        assert_eq!(store.count_rows("product_events"), 400);
        assert_eq!(
            store.query_text("SELECT DISTINCT session_id FROM product_events"),
            Some(new_session.to_string())
        );
        assert!(
            TelemetryStore::database_size_bytes(&database) <= size_both.saturating_sub(1),
            "cap must be satisfied after prune+checkpoint"
        );
        Ok(())
    }

    #[test]
    fn telemetry_wal_checkpoint_reports_busy_readers() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        let mut store = TelemetryStore::open(&paths)?;
        let session = SessionId::try_from(Uuid::from_u128(25))?;
        fill_session(&mut store, session, recent_timestamp()?, 8)?;
        let reader = Connection::open(paths.telemetry_database())?;
        reader.execute_batch("BEGIN")?;
        let _held: i64 =
            reader.query_row("SELECT COUNT(*) FROM product_events", [], |row| row.get(0))?;
        assert_eq!(store.checkpoint_wal()?, WalCheckpointOutcome::Busy);
        reader.execute_batch("COMMIT")?;
        assert_eq!(store.checkpoint_wal()?, WalCheckpointOutcome::Completed);
        Ok(())
    }

    #[test]
    fn telemetry_busy_reader_does_not_delete_sessions_to_chase_wal_bytes() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        let mut store = TelemetryStore::open(&paths)?;
        let session = SessionId::try_from(Uuid::from_u128(26))?;
        fill_session(&mut store, session, recent_timestamp()?, 8)?;
        let reader = Connection::open(paths.telemetry_database())?;
        reader.execute_batch("BEGIN")?;
        let _: i64 =
            reader.query_row("SELECT COUNT(*) FROM product_events", [], |row| row.get(0))?;
        let outcome = store.prune(
            TelemetryRetentionConfig {
                max_bytes: 1,
                max_age_days: 90,
            },
            &paths.telemetry_database(),
        )?;
        assert!(outcome.wal_checkpoint.is_busy());
        assert_eq!(store.count_rows("product_events"), 8);
        reader.execute_batch("COMMIT")?;
        Ok(())
    }

    #[test]
    fn telemetry_prune_keeps_sessions_with_unfinished_turns() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        let mut store = TelemetryStore::open(&paths)?;
        let session = SessionId::try_from(Uuid::from_u128(27))?;
        fill_session(&mut store, session, recent_timestamp()?, 8)?;
        store.connection.execute(
            "INSERT INTO turns(session_id, branch_id, run_id, agent_id, turn_id, started_at) VALUES (?1,'branch','run','agent','turn',1)",
            [session.to_string()],
        )?;
        store.prune(
            TelemetryRetentionConfig {
                max_bytes: 1,
                max_age_days: 1,
            },
            &paths.telemetry_database(),
        )?;
        assert_eq!(store.count_rows("product_events"), 8);
        assert_eq!(store.count_rows("turns"), 1);
        Ok(())
    }

    #[test]
    fn writer_refuses_foreign_schema_without_wiping() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        {
            let connection = Connection::open(paths.telemetry_database())?;
            connection.execute_batch(
                "CREATE TABLE turns (
                    session_id TEXT, branch_id TEXT, run_id TEXT, agent_id TEXT,
                    turn_id TEXT PRIMARY KEY, started_at INTEGER
                 );
                 PRAGMA user_version = 1;",
            )?;
            connection.execute("INSERT INTO turns VALUES ('s','b','r','a','t', 1)", [])?;
        }
        assert!(TelemetryStore::open(&paths).is_err());
        let connection = Connection::open(paths.telemetry_database())?;
        let count: i64 =
            connection.query_row("SELECT COUNT(*) FROM turns", [], |row| row.get(0))?;
        assert_eq!(count, 1);
        Ok(())
    }

    #[test]
    fn writer_leaves_legacy_and_other_schema_files_untouched() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        for (name, version) in [
            ("telemetry.sqlite3", 9),
            ("telemetry-v9.sqlite3", 9),
            ("telemetry-v18.sqlite3", 18),
        ] {
            let database = paths.telemetry_dir().join(name);
            let connection = Connection::open(&database)?;
            connection.execute_batch(&format!("CREATE TABLE evidence (value TEXT); INSERT INTO evidence VALUES ('retained'); PRAGMA user_version = {version};"))?;
            drop(connection);
            let before = std::fs::read(&database)?;
            let store = TelemetryStore::open(&paths)?;
            assert_eq!(store.query_i64("PRAGMA user_version"), Some(19));
            assert_eq!(
                store.query_i64("SELECT count(*) FROM sqlite_master WHERE name = 'evidence'"),
                Some(0)
            );
            assert_eq!(std::fs::read(&database)?, before);
        }
        Ok(())
    }

    #[test]
    fn read_paths_refuse_stale_schema_without_wiping() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        {
            let connection = Connection::open(paths.telemetry_database())?;
            connection.execute_batch(
                "CREATE TABLE turns (
                    session_id TEXT, branch_id TEXT, run_id TEXT, agent_id TEXT,
                    turn_id TEXT PRIMARY KEY, started_at INTEGER
                 );
                 PRAGMA user_version = 3;",
            )?;
            connection.execute("INSERT INTO turns VALUES ('s','b','r','a','t', 1)", [])?;
        }
        let error = TelemetryStore::query_attached(&paths, "SELECT COUNT(*) FROM turns")
            .expect_err("stale schema must fail closed");
        assert!(error.to_string().contains("file is left untouched"));
        assert_eq!(
            TelemetryStore::count_all_rows(&paths)
                .expect_err("stale")
                .to_string(),
            error.to_string()
        );
        let count: i64 = Connection::open(paths.telemetry_database())?.query_row(
            "SELECT COUNT(*) FROM turns",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(count, 1);
        Ok(())
    }

    #[test]
    fn turn_started_rows_default_cost_status_unknown() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        let mut store = TelemetryStore::open(&paths)?;
        let context = fixture_context()?;
        store.write_batch(&[TelemetryItem::Agent(TelemetryObservation::new(
            context.with_turn(TurnId::try_from(Uuid::from_u128(0x201)).expect("turn")),
            fixture_timestamp()?,
            TelemetryObservationKind::TurnStarted {
                origin: plakous::model::TurnOrigin::UserPrompt,
                queue_time: Duration::from_millis(1),
            },
        ))])?;
        assert_eq!(
            store.query_text("SELECT cost_status FROM turns"),
            Some("unknown".to_owned())
        );
        Ok(())
    }

    #[test]
    fn credential_refresh_transport_phase_fixture() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout()?;
        let store = TelemetryStore::open(&paths)?;
        store.connection.execute(
            "INSERT INTO credential_refreshes (at, provider, phase, failure_class, transport_phase, generation)
             VALUES (1720000100000000000, 'anthropic', 'restored', 'timeout', 'resolution', 2)",
            [],
        )?;
        assert_eq!(
            store.query_text("SELECT transport_phase FROM v_credential_refreshes"),
            Some("resolution".to_owned())
        );
        Ok(())
    }

    #[test]
    fn credential_refresh_round_trips_and_latest_query() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        let mut store = TelemetryStore::open(&paths)?;
        let context = fixture_context()?;
        let older = Timestamp::new(1_720_000_000, 0).expect("older");
        let newer = Timestamp::new(1_720_000_100, 0).expect("newer");
        store.write_batch(&[
            TelemetryItem::Agent(TelemetryObservation::new(
                context,
                older,
                TelemetryObservationKind::CredentialRefresh {
                    provider: plakous::agent::CredentialProviderLabel::new("xai").unwrap(),
                    phase: CredentialRefreshPhase::Began,
                    failure_class: None,
                    transport_phase: None,
                    http_status: None,
                    elapsed: None,
                    generation: 39,
                    detail: plakous::agent::CredentialRefreshDetail::default(),
                },
            )),
            TelemetryItem::Agent(TelemetryObservation::new(
                context,
                newer,
                TelemetryObservationKind::CredentialRefresh {
                    provider: plakous::agent::CredentialProviderLabel::new("xai").unwrap(),
                    phase: CredentialRefreshPhase::OutcomeUnknown,
                    failure_class: Some(CredentialRefreshFailureClass::Timeout),
                    transport_phase: Some(plakous::model::ModelRetryPhase::Resolution),
                    http_status: Some(503),
                    elapsed: Some(Duration::from_millis(30_000)),
                    generation: 40,
                    detail: plakous::agent::CredentialRefreshDetail {
                        reason: Some(plakous::agent::CredentialRefreshFailureReason::IdTokenExp),
                        expires_in_secs: Some(21_600),
                        id_token_iat_age_secs: Some(3_700),
                        id_token_exp_in_secs: Some(-100),
                    },
                },
            )),
        ])?;
        assert_eq!(
            store.query_i64("SELECT COUNT(*) FROM v_credential_refreshes"),
            Some(2)
        );
        assert_eq!(
            store.query_text(
                "SELECT http_status_class FROM v_credential_refreshes ORDER BY at DESC LIMIT 1"
            ),
            Some("5xx".to_owned())
        );
        let latest = TelemetryStore::latest_credential_refresh(&paths, "xai")
            .expect("query")
            .expect("row");
        assert_eq!(latest.phase, "outcome_unknown");
        assert_eq!(latest.failure_class.as_deref(), Some("timeout"));
        assert_eq!(latest.http_status, Some(503));
        assert_eq!(latest.generation, 40);
        assert_eq!(latest.transport_phase.as_deref(), Some("resolution"));
        assert_eq!(latest.failure_reason.as_deref(), Some("id_token_exp"));
        Ok(())
    }

    /// Unlinking an open sqlite file on Unix leaves the inode writable, so this
    /// cannot assert write errors; the contract is only that the agent-side
    /// emitter keeps accepting and the writer shuts down cleanly.
    #[test]
    fn deleting_database_mid_run_keeps_emitter_enabled_and_does_not_panic() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        let store = TelemetryStore::open(&paths)?;
        let database = paths.telemetry_database();
        let writer = TelemetryWriter::spawn(store, 16)?;
        let sink: Arc<dyn plakous::agent::TelemetrySink> =
            Arc::clone(&writer) as Arc<dyn plakous::agent::TelemetrySink>;
        let emitter = Arc::new(plakous::agent::TelemetryEmitter::enabled(sink));
        let observations = all_observations()?;
        emitter.emit(observations[0]);
        std::fs::remove_file(&database)?;
        let _ = std::fs::remove_file(database.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(database.with_extension("sqlite3-shm"));
        for observation in &observations[1..] {
            emitter.emit(*observation);
        }
        assert!(emitter.status().enabled());
        let _status = writer.close_and_join()?;
        assert!(emitter.status().enabled());
        Ok(())
    }

    #[test]
    fn compaction_refusal_reason_is_persisted_with_and_without_a_start() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        let mut store = TelemetryStore::open(&paths)?;
        let context = fixture_context()?;
        let at = fixture_timestamp()?;
        for started in [false, true] {
            if started {
                store.write_batch(&[TelemetryItem::Agent(TelemetryObservation::new(
                    context,
                    at,
                    TelemetryObservationKind::CompactionStarted {
                        reason: CompactionEventReason::Scheduled,
                    },
                ))])?;
            }
            store.write_batch(&[TelemetryItem::Agent(TelemetryObservation::new(
                context,
                at,
                TelemetryObservationKind::CompactionFinished {
                    refusal: Some(CompactionRefusalReason::Rejected),
                    elapsed: Duration::from_millis(1),
                    outcome: CompactionEventOutcome::Unavailable,
                    reason: CompactionEventReason::Scheduled,
                    tokens_before: Some(38_000),
                    tokens_after: None,
                    usage: None,
                },
            ))])?;
        }
        let count: i64 = store.connection.query_row(
            "SELECT count(*) FROM compactions WHERE refusal_reason = 'rejected' AND tokens_before = 38000", [], |row| row.get(0),
        )?;
        assert_eq!(count, 2);
        Ok(())
    }
}
