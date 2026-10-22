//! `plak telemetry` CLI.

use std::io::Write;

use crate::home::paths::PlakPaths;
use crate::product_config::TelemetryConfig;
use crate::telemetry_sink::telemetry_writer_status;
use crate::telemetry_store::{TelemetryRetentionConfig, TelemetryStore, TelemetryStoreStatus};

use crate::cli_output::{HumanOutput, Table, TerminalMode};
use crate::{CliFailure, CliResult, ProcessOutcome};

const RECOMMENDED_VIEWS: &str = r"
-- Turn token costs and TTFT (telemetry only)
SELECT * FROM v_turn_costs ORDER BY elapsed_ns DESC LIMIT 20;

-- Discarded partial-response replay loss per retry (content-free)
SELECT * FROM v_replay_loss ORDER BY at DESC LIMIT 20;

-- Per-agent token totals
SELECT * FROM v_agent_tokens ORDER BY input_tokens DESC;

-- Tool success counts per turn
SELECT * FROM v_tool_success ORDER BY total DESC LIMIT 20;

-- Child/sidecar CPU and peak RSS per session
SELECT * FROM v_child_resources ORDER BY user_cpu_ns DESC;

-- Model provenance per plakous session (TEMP view built after ATTACH product and subagents;
-- root sessions from product.sessions, spawned children from subagents.agent_nodes)
SELECT * FROM v_session_models;

-- Cache hit rate per model/effort/composition
SELECT
  s.tuple_id AS model, s.effort, s.composition,
  COUNT(*) AS turns,
  SUM(t.cached_input_tokens) AS cached_input,
  SUM(t.cache_write_tokens) AS cache_write,
  SUM(t.input_tokens) AS input_tokens,
  ROUND(100.0 * SUM(t.cached_input_tokens) / NULLIF(SUM(t.input_tokens), 0), 2) AS cache_hit_pct
FROM v_turn_costs t
JOIN v_session_models s ON s.session_id = t.session_id
GROUP BY s.tuple_id, s.effort, s.composition
ORDER BY cache_hit_pct DESC;

-- Average TTFT / TTLB per model
SELECT
  s.tuple_id AS model, s.effort,
  COUNT(*) AS attempts,
  AVG(m.ttft_ns) / 1e6 AS avg_ttft_ms,
  AVG(m.ttlb_ns) / 1e6 AS avg_ttlb_ms
FROM model_attempts m
JOIN v_session_models s ON s.session_id = m.session_id
WHERE m.ttft_ns IS NOT NULL
GROUP BY s.tuple_id, s.effort
ORDER BY avg_ttft_ms;

-- Cost per session and served-endpoint mix (OpenRouter Chat reports both)
SELECT
  m.session_id,
  m.served_endpoint,
  COUNT(*) AS attempts,
  SUM(m.cost_micros) AS cost_micros,
  ROUND(SUM(m.cost_micros) / 1000000.0, 6) AS cost_usd,
  SUM(CASE WHEN m.cost_status = 'reported' THEN 1 ELSE 0 END) AS reported_attempts
FROM model_attempts m
WHERE m.finished_at IS NOT NULL
GROUP BY m.session_id, m.served_endpoint
ORDER BY cost_micros DESC;

-- Tool success rate per agent
SELECT
  v.agent_id,
  SUM(v.completed) AS completed,
  SUM(v.total) AS total,
  ROUND(100.0 * SUM(v.completed) / NULLIF(SUM(v.total), 0), 2) AS success_pct
FROM v_tool_success v
GROUP BY v.agent_id
ORDER BY success_pct DESC;
";

#[derive(Debug, Clone)]
pub(crate) enum TelemetryCommand {
    Status {
        views: bool,
    },
    Sql {
        query: String,
        json: bool,
    },
    Prune {
        max_age_days: Option<u64>,
        max_bytes: Option<u64>,
    },
}

pub(crate) fn run_telemetry(
    paths: &PlakPaths,
    command: TelemetryCommand,
    stdout: &mut dyn Write,
) -> CliResult<ProcessOutcome> {
    match command {
        TelemetryCommand::Status { views } => telemetry_status(paths, views, stdout),
        TelemetryCommand::Sql { query, json } => telemetry_sql(paths, &query, json, stdout),
        TelemetryCommand::Prune {
            max_age_days,
            max_bytes,
        } => telemetry_prune(paths, max_age_days, max_bytes, stdout),
    }
}

fn telemetry_status(
    paths: &PlakPaths,
    views: bool,
    stdout: &mut dyn Write,
) -> CliResult<ProcessOutcome> {
    let database = paths.telemetry_database();
    writeln!(stdout, "Telemetry store").map_err(|_| CliFailure::new("stdout failed"))?;
    writeln!(stdout, "  Path           {}", database.display())
        .map_err(|_| CliFailure::new("stdout failed"))?;
    if !database.exists() {
        writeln!(stdout, "  Status         missing")
            .map_err(|_| CliFailure::new("stdout failed"))?;
        return write_recommended_views(views, stdout);
    }
    let retention = TelemetryRetentionConfig::from(TelemetryConfig::load(paths));
    let size = TelemetryStore::database_size_bytes(&database);
    let store = TelemetryStore::open(paths)
        .map_err(|error| CliFailure::new(format!("telemetry store unavailable: {error}")))?;
    let status: TelemetryStoreStatus = store
        .status_snapshot()
        .map_err(|error| CliFailure::new(format!("telemetry status failed: {error}")))?;
    let writer = telemetry_writer_status(paths);
    let write_errors = writer.write_errors();
    let dropped = writer.dropped();
    writeln!(stdout, "  Schema version {}", status.schema_version)
        .map_err(|_| CliFailure::new("stdout failed"))?;
    writeln!(stdout, "  Size on disk    {} bytes", size)
        .map_err(|_| CliFailure::new("stdout failed"))?;
    writeln!(
        stdout,
        "  Retention       {} bytes, {} days",
        retention.max_bytes, retention.max_age_days
    )
    .map_err(|_| CliFailure::new("stdout failed"))?;
    writeln!(stdout, "  Write errors    {write_errors}")
        .map_err(|_| CliFailure::new("stdout failed"))?;
    writeln!(stdout, "  Dropped         {dropped}")
        .map_err(|_| CliFailure::new("stdout failed"))?;
    writeln!(
        stdout,
        "  WAL checkpoint busy  {}",
        writer.wal_checkpoint_busy()
    )
    .map_err(|_| CliFailure::new("stdout failed"))?;
    let mut table = Table::new(["TABLE", "ROWS"]);
    for (name, count) in status.table_counts {
        table
            .push([name, count.to_string()])
            .map_err(CliFailure::new)?;
    }
    HumanOutput::resolve(
        TerminalMode::Interactive,
        None,
        crate::cli_output::DetailLevel::default(),
    )
    .write_table(stdout, &table)
    .map_err(|_| CliFailure::new("stdout failed"))?;
    write_recommended_views(views, stdout)
}

/// `--views` folds the former `plak telemetry views` verb into `status`.
fn write_recommended_views(views: bool, stdout: &mut dyn Write) -> CliResult<ProcessOutcome> {
    if views {
        writeln!(
            stdout,
            "\nRECOMMENDED VIEWS (paste into `plak telemetry sql`):"
        )
        .and_then(|()| writeln!(stdout, "{RECOMMENDED_VIEWS}"))
        .map_err(|_| CliFailure::new("stdout failed"))?;
    }
    Ok(ProcessOutcome::Success)
}

fn telemetry_sql(
    paths: &PlakPaths,
    query: &str,
    json: bool,
    stdout: &mut dyn Write,
) -> CliResult<ProcessOutcome> {
    let rows = TelemetryStore::query_attached(paths, query)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    if json {
        writeln!(
            stdout,
            "{}",
            serde_json::to_string(&rows).unwrap_or_else(|_| "[]".into())
        )
        .map_err(|_| CliFailure::new("stdout failed"))?;
    } else {
        for row in rows {
            writeln!(stdout, "{}", row.join("\t")).map_err(|_| CliFailure::new("stdout failed"))?;
        }
    }
    Ok(ProcessOutcome::Success)
}

fn telemetry_prune(
    paths: &PlakPaths,
    max_age_days: Option<u64>,
    max_bytes: Option<u64>,
    stdout: &mut dyn Write,
) -> CliResult<ProcessOutcome> {
    let mut retention = TelemetryRetentionConfig::from(TelemetryConfig::load(paths));
    if let Some(days) = max_age_days {
        retention.max_age_days = days;
    }
    if let Some(bytes) = max_bytes {
        retention.max_bytes = bytes;
    }
    let mut store =
        TelemetryStore::open(paths).map_err(|error| CliFailure::new(error.to_string()))?;
    let outcome = store
        .prune(retention, &paths.telemetry_database())
        .map_err(|error| CliFailure::new(format!("telemetry prune failed: {error}")))?;
    writeln!(stdout, "Telemetry pruned (whole sessions, oldest first).")
        .map_err(|_| CliFailure::new("stdout failed"))?;
    if outcome.wal_checkpoint.is_busy() {
        writeln!(
            stdout,
            "WAL checkpoint TRUNCATE was busy; another reader still holds the log."
        )
        .map_err(|_| CliFailure::new("stdout failed"))?;
    }
    Ok(ProcessOutcome::Success)
}
