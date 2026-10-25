use crate::observation_projection::{Cell, Row, RowKind};

pub(super) fn write(
    transaction: &rusqlite::Transaction<'_>,
    row: Row,
) -> Result<(), rusqlite::Error> {
    let count = transaction.execute(
        statement(row.kind),
        rusqlite::params_from_iter(row.values.iter()),
    )?;
    if count == 0
        && let Some(fallback) = row.fallback
    {
        transaction.execute(
            statement(RowKind::UnpairedCompactionFinished),
            rusqlite::params_from_iter(fallback.iter()),
        )?;
    }
    Ok(())
}

impl rusqlite::ToSql for Cell {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        use rusqlite::types::{ToSqlOutput, ValueRef};
        Ok(ToSqlOutput::Borrowed(match self {
            Cell::Null => ValueRef::Null,
            Cell::Integer(value) => ValueRef::Integer(*value),
            Cell::Text(value) => ValueRef::Text(value.as_bytes()),
        }))
    }
}

fn statement(kind: RowKind) -> &'static str {
    match kind {
        RowKind::Job => "INSERT INTO job_events (at, session_id, branch_id, run_id, agent_id, turn_id, call_id, kind, tool, nested, hint_source, hint_ms, delivery_point, count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        RowKind::TurnStarted => "INSERT INTO turns (
                    session_id, branch_id, run_id, agent_id, turn_id,
                    started_at, queue_ns, origin
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(turn_id) DO UPDATE SET
                    started_at = excluded.started_at,
                    run_id = CASE WHEN turns.run_id = '' THEN excluded.run_id ELSE turns.run_id END,
                    agent_id = CASE WHEN turns.agent_id = '' THEN excluded.agent_id ELSE turns.agent_id END,
                    queue_ns = excluded.queue_ns,
                    origin = excluded.origin",
        RowKind::TurnFinished => "INSERT INTO turns (
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
        RowKind::ModelStarted => "INSERT INTO model_attempts (
                    attempt_id, session_id, branch_id, run_id, agent_id, turn_id, started_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(attempt_id) DO UPDATE SET
                    started_at = excluded.started_at",
        RowKind::ModelFirstEvent => "INSERT INTO model_attempts (
                    attempt_id, session_id, branch_id, run_id, agent_id, turn_id,
                    first_event_at, ttft_ns
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(attempt_id) DO UPDATE SET
                    first_event_at = excluded.first_event_at,
                    ttft_ns = excluded.ttft_ns",
        RowKind::ModelStreamActivity => "INSERT INTO model_attempts (attempt_id, session_id, branch_id, run_id, agent_id, turn_id, last_semantic_event_at, keepalive_count, max_semantic_gap_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(attempt_id) DO UPDATE SET last_semantic_event_at = COALESCE(excluded.last_semantic_event_at, model_attempts.last_semantic_event_at), keepalive_count = MAX(model_attempts.keepalive_count, excluded.keepalive_count), max_semantic_gap_ms = MAX(model_attempts.max_semantic_gap_ms, excluded.max_semantic_gap_ms)",
        RowKind::ModelFinished => "INSERT INTO model_attempts (
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
        RowKind::ToolStarted => "INSERT INTO tool_calls (
                    call_id, session_id, branch_id, run_id, agent_id, turn_id,
                    attempt_id, started_at, tool_name
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(turn_id, call_id) DO UPDATE SET
                    started_at = excluded.started_at,
                    tool_name = excluded.tool_name",
        RowKind::ToolFinished => "INSERT INTO tool_calls (
                    call_id, session_id, branch_id, run_id, agent_id, turn_id,
                    attempt_id, finished_at, elapsed_ns, status, output_bytes
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(turn_id, call_id) DO UPDATE SET
                    finished_at = excluded.finished_at,
                    elapsed_ns = excluded.elapsed_ns,
                    status = excluded.status,
                    output_bytes = excluded.output_bytes",
        RowKind::CompactionStarted => "INSERT INTO compactions (
                    session_id, branch_id, run_id, agent_id, turn_id,
                    started_at, reason
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        RowKind::CompactionFinished => "UPDATE compactions SET
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
        RowKind::UnpairedCompactionFinished => "INSERT INTO compactions (
                        session_id, branch_id, run_id, agent_id, turn_id,
                        finished_at, elapsed_ns, reason, outcome,
                        tokens_before, tokens_after,
                        input_tokens, output_tokens, reasoning_tokens,
                        cached_input_tokens, cache_write_tokens,
                        cost_micros, cost_status, refusal_reason
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
        RowKind::DurabilityFinished => "INSERT INTO durability_attempts (
                    session_id, branch_id, run_id, agent_id, turn_id,
                    at, elapsed_ns, kind, status
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        RowKind::DurabilityAttemptFinished => "INSERT INTO durability_attempts (
                    session_id, branch_id, run_id, agent_id, turn_id,
                    at, elapsed_ns, kind, attempt, outcome
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        RowKind::RetentionFinished => "INSERT INTO retention_runs (
                    session_id, branch_id, run_id, agent_id, turn_id, at, elapsed_ns,
                    status, pruned_checkpoints
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        RowKind::ChildProcessExited => "INSERT INTO resource_samples (
                    at, session_id, agent_id, phase, child_kind, rss_bytes, peak_rss_bytes,
                    user_cpu_ns, system_cpu_ns, threads, fds
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        RowKind::PromptCacheDiagnosis => "INSERT INTO prompt_cache_diagnoses (
                    at, session_id, branch_id, run_id, agent_id, turn_id, attempt_id,
                    reason, missed_input_tokens
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        RowKind::CredentialRefresh => "INSERT INTO credential_refreshes (
                    at, provider, phase, failure_class, http_status, elapsed_ns, generation, transport_phase,
                    failure_reason
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    }
}
