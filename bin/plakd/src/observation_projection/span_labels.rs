use super::Status;
use super::labels::credential_refresh_phase_text;
pub(super) use super::labels::{
    compaction_outcome_text as compaction_outcome, compaction_reason_text as compaction_reason,
    durability_attempt_kind_text as durability_attempt,
    durability_attempt_outcome_text as durability_outcome, terminal_status_text as terminal_status,
};
use plakous::agent::{
    ChildProcessKind, CompactionEventOutcome, CredentialRefreshPhase, DurabilityAttemptOutcome,
    TelemetryTerminalStatus,
};

pub(super) fn span_status(status: TelemetryTerminalStatus) -> Status {
    match status {
        TelemetryTerminalStatus::Completed => Status::Ok,
        _ => Status::error(terminal_status(status)),
    }
}

pub(super) fn credential_refresh_span_status(phase: CredentialRefreshPhase) -> Status {
    #[allow(
        unreachable_patterns,
        reason = "non-exhaustive telemetry enums reserve future variants"
    )]
    match phase {
        CredentialRefreshPhase::OutcomeUnknown | CredentialRefreshPhase::Quarantined => {
            Status::error(credential_refresh_phase_text(phase))
        }
        _ => Status::Ok,
    }
}

pub(super) fn compaction_span_status(outcome: CompactionEventOutcome) -> Status {
    #[allow(
        unreachable_patterns,
        reason = "non-exhaustive telemetry enums reserve future variants"
    )]
    match outcome {
        CompactionEventOutcome::Committed | CompactionEventOutcome::NotNeeded => Status::Ok,
        _ => Status::error(compaction_outcome(outcome)),
    }
}

pub(super) fn durability_attempt_span_status(outcome: DurabilityAttemptOutcome) -> Status {
    match outcome {
        DurabilityAttemptOutcome::Committed
        | DurabilityAttemptOutcome::RecoveredAcknowledgement
        | DurabilityAttemptOutcome::Joined => Status::Ok,
        _ => Status::error(durability_outcome(outcome)),
    }
}

pub(super) fn child_process_kind(child: ChildProcessKind) -> &'static str {
    match child {
        ChildProcessKind::JsEvalSidecar => "js_eval_sidecar",
        ChildProcessKind::PythonWorker => "python_worker",
        _ => "unknown",
    }
}
