use plakous::agent::{
    CompactionEventOutcome, CompactionEventReason, CredentialRefreshFailureClass,
    CredentialRefreshPhase, DurabilityAttemptKind, DurabilityAttemptOutcome, HttpStatusClass,
    TelemetryTerminalStatus,
};
use plakous::model::{Timestamp, Usage};
use std::time::Duration;
type UsageColumns = (
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

pub(crate) fn usage_columns(usage: Usage) -> UsageColumns {
    (
        i64::try_from(usage.input_tokens()).ok(),
        i64::try_from(usage.output_tokens()).ok(),
        i64::try_from(usage.reasoning_tokens()).ok(),
        i64::try_from(usage.cached_input_tokens()).ok(),
        i64::try_from(usage.cache_write_tokens()).ok(),
    )
}

pub(crate) fn cost_fields_from_usage(usage: Usage) -> (Option<i64>, &'static str) {
    match usage.cost_micros() {
        Some(micros) => (Some(i64::try_from(micros).unwrap_or(i64::MAX)), "reported"),
        None => (None, "unknown"),
    }
}

pub(crate) fn id_text<T: std::fmt::Display>(id: T) -> String {
    id.to_string()
}

pub(crate) fn row_timestamp_ns(timestamp: Timestamp) -> i64 {
    let seconds = i64::try_from(timestamp.seconds()).unwrap_or(i64::MAX);
    seconds
        .saturating_mul(1_000_000_000)
        .saturating_add(i64::from(timestamp.nanoseconds()))
}

pub(crate) fn row_duration_ns(duration: Duration) -> i64 {
    i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX)
}

pub(crate) fn terminal_status_text(status: TelemetryTerminalStatus) -> &'static str {
    match status {
        TelemetryTerminalStatus::Completed => "completed",
        TelemetryTerminalStatus::Failed => "failed",
        TelemetryTerminalStatus::Cancelled => "cancelled",
        TelemetryTerminalStatus::Shutdown => "shutdown",
        _ => "unknown",
    }
}

pub(crate) fn durability_attempt_kind_text(kind: DurabilityAttemptKind) -> &'static str {
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

pub(crate) fn compaction_reason_text(reason: CompactionEventReason) -> &'static str {
    match reason {
        CompactionEventReason::Manual => "manual",
        CompactionEventReason::Scheduled => "scheduled",
    }
}

pub(crate) fn compaction_outcome_text(outcome: CompactionEventOutcome) -> &'static str {
    match outcome {
        CompactionEventOutcome::Committed => "committed",
        CompactionEventOutcome::Failed => "failed",
        CompactionEventOutcome::RecoveryRequired => "recovery_required",
        CompactionEventOutcome::NotNeeded => "not_needed",
        CompactionEventOutcome::NoEligibleRange => "no_eligible_range",
        CompactionEventOutcome::Unavailable => "unavailable",
    }
}

pub(crate) fn durability_attempt_outcome_text(outcome: DurabilityAttemptOutcome) -> &'static str {
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
