//! Disposable local analytics store for typed plakous telemetry observations.

use std::path::{Path, PathBuf};
use std::time::Duration;

use plakous::agent::TelemetryObservation;
#[cfg(test)]
use plakous::agent::{
    CompactionEventOutcome, CompactionEventReason, CompactionRefusalReason,
    CredentialRefreshFailureClass, CredentialRefreshPhase,
};
use plakous::model::Timestamp;
use rusqlite::{Connection, OpenFlags, params};

use crate::home::paths::PlakPaths;
use crate::observation_projection::labels::{
    cost_fields_from_usage, id_text, row_duration_ns as duration_ns,
    row_timestamp_ns as timestamp_ns,
};
pub use crate::observation_projection::labels::{
    credential_refresh_failure_text, credential_refresh_phase_text,
};
#[cfg(test)]
use crate::observation_projection::labels::{
    durability_attempt_kind_text, durability_attempt_outcome_text,
};
use crate::observation_projection::{Output, project};
use plakous_hosted::product_observation::{ChildKind, ProductObservation};

pub(crate) const TELEMETRY_SCHEMA_VERSION: i64 = 22;

/// Default disposable telemetry retention.
pub const DEFAULT_TELEMETRY_MAX_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_TELEMETRY_MAX_AGE_DAYS: u64 = 90;
pub(crate) mod files;
mod observation;
#[cfg(all(test, feature = "otel"))]
mod projection_tests;
pub(crate) mod recorded_usage;

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
  prepared_input_high_water INTEGER,
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
  cost_status TEXT NOT NULL DEFAULT 'unknown',
  context_window_tokens INTEGER,
  instruction_bytes INTEGER,
  tool_schema_bytes INTEGER,
  prepared_input_tokens INTEGER,
  idle_gap_ns INTEGER
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
  access_digest TEXT,
  range_first_line INTEGER,
  range_lines INTEGER,
  page_partial INTEGER,
  observation_clipped_bytes INTEGER,
  command_class TEXT,
  wait_count INTEGER,
  wait_kind TEXT,
  wait_outcome TEXT,
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
  cost_status TEXT NOT NULL DEFAULT 'unknown',
  context_window_tokens INTEGER,
  threshold_tokens INTEGER,
  occupancy_tokens INTEGER,
  idle_gap_ns INTEGER
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
  account TEXT,
  phase TEXT NOT NULL,
  failure_class TEXT,
  transport_phase TEXT,
  http_status INTEGER,
  elapsed_ns INTEGER,
  generation INTEGER NOT NULL,
  failure_reason TEXT,
  oauth_error_code TEXT,
  oauth_error_description TEXT
);
CREATE INDEX credential_refreshes_account_at ON credential_refreshes(account, at);

CREATE TABLE recorded_terminals (
  turn_id TEXT PRIMARY KEY NOT NULL,
  session_id TEXT NOT NULL,
  branch_id TEXT NOT NULL,
  committed_at_ms INTEGER NOT NULL CHECK(committed_at_ms >= 0),
  account TEXT,
  input_tokens TEXT NOT NULL,
  output_tokens TEXT NOT NULL,
  reasoning_tokens TEXT NOT NULL,
  cached_input_tokens TEXT NOT NULL,
  cache_write_tokens TEXT NOT NULL,
  model_operations TEXT NOT NULL,
  reporting_operations TEXT NOT NULL,
  cost_micros TEXT
);
CREATE INDEX recorded_terminals_window ON recorded_terminals(committed_at_ms, account);
CREATE INDEX recorded_terminals_account_window ON recorded_terminals(account, committed_at_ms);
CREATE INDEX recorded_terminals_session ON recorded_terminals(session_id);

CREATE TABLE recording_evidence (
  singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
  started_at_ms INTEGER NOT NULL,
  retained_from_ms INTEGER,
  dropped INTEGER NOT NULL DEFAULT 0,
  write_errors INTEGER NOT NULL DEFAULT 0
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
  account,
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
  oauth_error_code,
  oauth_error_description,
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
    record: Option<&recorded_usage::TurnAttribution>,
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
                account: record.and_then(|record| recorded_usage::account_for(record, terminal)),
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
    "recorded_terminals",
    "recording_evidence",
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
    "recorded_terminals",
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
        store
            .connection
            .execute(
                "INSERT OR IGNORE INTO recording_evidence(singleton, started_at_ms) VALUES(1, ?1)",
                [recorded_usage::now_ms()],
            )
            .map_err(|_| {
                TelemetryStoreOpenError::unavailable("could not initialize recording evidence")
            })?;
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
                    tuple_id, account, effort, NULL, 'child', template
             FROM subagents.agent_nodes
             WHERE session_locator IS NOT NULL AND instr(session_locator, '/') > 1"
    } else {
        ""
    };
    connection
        .execute_batch(&format!(
            "CREATE TEMP VIEW {SESSION_MODELS_VIEW} AS
             SELECT substr(locator, 1, instr(locator, '/') - 1) AS session_id,
                    model AS tuple_id, account, effort, composition, 'root' AS origin,
                    NULL AS template
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
    let transaction = connection.unchecked_transaction()?;
    // This watermark is evidence of loss, not proof that every producer was
    // observed above it. Retain it even after the deleted session is gone.
    transaction.execute(
        "UPDATE recording_evidence SET retained_from_ms = MAX(COALESCE(retained_from_ms, 0), COALESCE((SELECT MAX(committed_at_ms) + 1 FROM recorded_terminals WHERE session_id = ?1), 0)) WHERE singleton = 1",
        [session_id],
    )?;
    for table in SESSION_PRUNE_TABLES {
        transaction.execute(
            &format!("DELETE FROM {table} WHERE session_id = ?1"),
            [session_id],
        )?;
    }
    transaction.commit()
}

#[allow(
    clippy::too_many_lines,
    reason = "Product observation writing matches every row shape to its SQL bindings inside the current telemetry transaction."
)]
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
            recorded_usage::write_terminal(transaction, observation)?;
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
        ProductObservation::StartupTiming {
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
    if let Some(row) = project(observation, Output::Rows).row {
        observation::write(transaction, row)?;
    }
    Ok(())
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
    pub oauth_error_code: Option<String>,
    pub oauth_error_description: Option<String>,
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
                    failure_reason, oauth_error_code, oauth_error_description
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
                    oauth_error_code: row.get(9)?,
                    oauth_error_description: row.get(10)?,
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
mod tests;
