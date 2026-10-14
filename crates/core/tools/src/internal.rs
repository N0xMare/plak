//! Unsupported sibling seams for official agent and hosting implementations.
//!
//! These preserve one-way dependencies while keeping implementation details
//! out of the supported tools inventory.

#![allow(
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    missing_docs,
    reason = "unsupported sibling seam is documented as one implementation tier"
)]

use std::num::NonZeroU64;
use std::sync::Arc;

pub mod preview;

use plakous_model::{AttemptId, CancellationToken, Clock, Digest, RunId, Timestamp, TurnId};

use crate::{
    EffectAuthorizationBinding, EffectAuthorizationGrant, EffectAuthorizationGrantSource,
    EffectDecision, EffectDecisionDisposition, ExecutionEnvironment,
    ExecutionEnvironmentDescriptor, FrozenToolRegistry, ProgressSink, ToolContext, ToolExecutor,
    ToolInvocation, ToolOutcome, ToolOutcomeLimits, ToolPolicyRequest, ToolPolicyRevision,
    ToolRuntimeError,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessSessionUpdate {
    Promoted {
        session_id: [u8; 16],
        owner_generation: NonZeroU64,
        number: u32,
    },
    JobTerminal {
        number: u32,
    },
    JobTerminals {
        numbers: Vec<u32>,
    },
    Active {
        session_id: [u8; 16],
        owner_generation: NonZeroU64,
    },
    Joined {
        session_id: [u8; 16],
        owner_generation: NonZeroU64,
    },
}

/// Hydrates durable rows as read-only snapshots; never reconstructs a live process.
pub fn restore_job_terminals(sessions: &crate::JoinedProcessSessions, views: &[crate::JobView]) {
    sessions.restore_job_terminals(views);
}

pub fn sanitize_job_command(command: &str) -> String {
    let mut text = String::new();
    for ch in command.chars() {
        match ch {
            '\n' => text.push_str("\\n"),
            '\r' => text.push_str("\\r"),
            ch if ch.is_control() => text.push('\u{fffd}'),
            ch => text.push(ch),
        }
    }
    let mut end = text.len().min(1024);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

pub fn seed_job_counter(sessions: &crate::JoinedProcessSessions, next_id: u32) {
    sessions.seed_job_counter(next_id);
}

pub fn set_job_owner(sessions: &crate::JoinedProcessSessions, owner: String) {
    sessions.set_job_owner(owner);
}

pub fn close_job_admission(sessions: &crate::JoinedProcessSessions, owner: &str) {
    sessions.close_job_admission(owner);
}

pub fn stop_jobs_for_owner(
    sessions: &crate::JoinedProcessSessions,
    owner: &str,
    by: crate::JobStopReason,
    grace: std::time::Duration,
) -> Result<(), crate::ExecutionBackendError> {
    sessions.stop_jobs_for_owner(owner, by, grace)
}

pub fn next_job_number(sessions: &crate::JoinedProcessSessions) -> u32 {
    sessions.next_job_number()
}

pub fn promote(
    sessions: &crate::JoinedProcessSessions,
    id: crate::JoinedProcessSessionId,
) -> Result<(), crate::ExecutionBackendError> {
    sessions.promote(id)
}

/// Room under the jobs root for one promoted job's output file.
#[derive(Debug)]
#[must_use]
pub struct JobOutputReservation {
    fits: bool,
}

impl JobOutputReservation {
    const fn into_fits(self) -> bool {
        self.fits
    }
}

/// Sweeps and budgets the jobs root for one promotion of `id`, unless the
/// promotion would be refused now. It walks the root: async callers run it on
/// the blocking pool, then [`promote_reserved`] inline.
pub fn reserve_job_output(
    sessions: &crate::JoinedProcessSessions,
    id: crate::JoinedProcessSessionId,
) -> JobOutputReservation {
    JobOutputReservation {
        fits: sessions.reserve_job_output(id),
    }
}

/// Promotes without walking the jobs root, so an async caller publishes the
/// job in the same poll that produces its result and cancellation cannot
/// drop the caller after the job's ownership moved to the job table.
pub fn promote_reserved(
    sessions: &crate::JoinedProcessSessions,
    id: crate::JoinedProcessSessionId,
    reservation: JobOutputReservation,
) -> Result<(), crate::ExecutionBackendError> {
    sessions.promote_reserved(id, reservation.into_fits())
}

pub fn consume_jobs(sessions: &crate::JoinedProcessSessions, numbers: &[u32]) {
    sessions.consume_jobs(numbers);
}

/// Bounds and sanitizes a terminal tail without letting process controls enter a checkpoint.
pub fn job_output_tail(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "");
    let text = normalized.as_str();
    let text = text.strip_suffix('\n').unwrap_or(text);
    let mut reversed = Vec::new();
    let mut bytes = 0;
    let mut lines = 1;
    for ch in text.chars().rev() {
        if ch == '\n' {
            if lines == 40 {
                break;
            }
            lines += 1;
        }
        let ch = if ch.is_control() && ch != '\n' && ch != '\t' {
            '\u{fffd}'
        } else {
            ch
        };
        if bytes + ch.len_utf8() > 4096 {
            break;
        }
        bytes += ch.len_utf8();
        reversed.push(ch);
    }
    reversed.into_iter().rev().collect()
}

fn make_process_session_update(
    sessions: &crate::JoinedProcessSessions,
    session_id: crate::JoinedProcessSessionId,
    joined: bool,
) -> ProcessSessionUpdate {
    let owner_generation = sessions.runtime_owner_generation();
    if sessions.is_promoted(session_id) {
        let number = crate::JoinedProcessSessions::job_number(session_id);
        if joined {
            ProcessSessionUpdate::JobTerminal { number }
        } else {
            ProcessSessionUpdate::Promoted {
                session_id: session_id.runtime_bytes(),
                owner_generation,
                number,
            }
        }
    } else if joined {
        ProcessSessionUpdate::Joined {
            session_id: session_id.runtime_bytes(),
            owner_generation,
        }
    } else {
        ProcessSessionUpdate::Active {
            session_id: session_id.runtime_bytes(),
            owner_generation,
        }
    }
}

pub fn process_session_active(
    outcome: ToolOutcome,
    sessions: &crate::JoinedProcessSessions,
    session_id: crate::JoinedProcessSessionId,
) -> ToolOutcome {
    outcome.with_process_session_update(make_process_session_update(sessions, session_id, false))
}

pub fn process_session_joined(
    outcome: ToolOutcome,
    sessions: &crate::JoinedProcessSessions,
    session_id: crate::JoinedProcessSessionId,
) -> ToolOutcome {
    outcome.with_process_session_update(make_process_session_update(sessions, session_id, true))
}

/// Starts a process whose full output is spooled only once its first page
/// would truncate (or its in-memory window would evict output).
pub fn start_process_with_output_limit(
    sessions: &crate::JoinedProcessSessions,
    request: crate::JoinedProcessStartRequest,
    control: crate::ExecutionOperationControl,
    max_output_bytes: usize,
) -> Result<crate::JoinedProcessSessionId, crate::ExecutionBackendError> {
    sessions.start_with_output_limit(request, control, max_output_bytes)
}

pub fn process_job_terminals(outcome: ToolOutcome, numbers: Vec<u32>) -> ToolOutcome {
    if numbers.is_empty() {
        outcome
    } else {
        outcome.with_process_session_update(ProcessSessionUpdate::JobTerminals { numbers })
    }
}

pub fn process_session_update(outcome: &ToolOutcome) -> Option<ProcessSessionUpdate> {
    outcome.process_session_update()
}

pub type ProcessSessionProjectionState = (
    Arc<std::sync::Mutex<crate::JoinedProcessOutputCursor>>,
    Arc<std::sync::Mutex<Vec<u8>>>,
);

pub fn process_session_projection_state(
    sessions: &crate::JoinedProcessSessions,
    id: crate::JoinedProcessSessionId,
) -> Result<ProcessSessionProjectionState, crate::ExecutionBackendError> {
    sessions.projection_state(id)
}

pub fn process_session_tail(
    sessions: &crate::JoinedProcessSessions,
    id: crate::JoinedProcessSessionId,
    max_bytes: usize,
    max_records: usize,
) -> Result<crate::JoinedProcessCursorPollResult, crate::ExecutionBackendError> {
    sessions.poll_tail(id, max_bytes, max_records)
}

/// Subscribe before polling to observe output and status without lost wakeups.
pub fn process_session_changes(
    sessions: &crate::JoinedProcessSessions,
    id: crate::JoinedProcessSessionId,
) -> Result<tokio::sync::watch::Receiver<u64>, crate::ExecutionBackendError> {
    sessions.session_changes(id)
}

pub fn reported_process_session(
    sessions: &crate::JoinedProcessSessions,
    number: u32,
) -> Option<(crate::JoinedProcessSessionId, crate::JoinedProcessStatus)> {
    sessions.reported_session_for_number(number)
}

pub const fn process_session_identity(id: crate::JoinedProcessSessionId) -> [u8; 16] {
    id.runtime_bytes()
}

pub fn active_process_session_identities(
    sessions: &crate::JoinedProcessSessions,
) -> Vec<([u8; 16], NonZeroU64)> {
    let generation = sessions.runtime_owner_generation();
    sessions
        .active_session_ids()
        .into_iter()
        .map(|id| (id.runtime_bytes(), generation))
        .collect()
}

pub fn joined_process_session_identities(
    sessions: &crate::JoinedProcessSessions,
) -> Vec<([u8; 16], NonZeroU64)> {
    let generation = sessions.runtime_owner_generation();
    sessions
        .joined_terminal_identities()
        .into_iter()
        .map(|id| (id, generation))
        .collect()
}

pub fn policy_request(
    invocation: ToolInvocation,
    definition: plakous_model::ToolDefinition,
    environment: Arc<ExecutionEnvironment>,
    policy_revision: ToolPolicyRevision,
    authorization_binding: EffectAuthorizationBinding,
) -> ToolPolicyRequest {
    ToolPolicyRequest::new(
        invocation,
        definition,
        environment,
        policy_revision,
        authorization_binding,
    )
}

pub fn authorization_binding(
    invocation: &ToolInvocation,
    environment_descriptor: ExecutionEnvironmentDescriptor,
    policy_revision: ToolPolicyRevision,
) -> Result<EffectAuthorizationBinding, ToolRuntimeError> {
    EffectAuthorizationBinding::for_invocation(invocation, environment_descriptor, policy_revision)
}

pub fn effect_decision_disposition(decision: EffectDecision) -> EffectDecisionDisposition {
    decision.into_disposition()
}

pub const fn authorization_grant(
    binding: EffectAuthorizationBinding,
    source: EffectAuthorizationGrantSource,
) -> EffectAuthorizationGrant {
    EffectAuthorizationGrant::new(binding, source)
}

pub fn validate_outcome_limits(
    limits: ToolOutcomeLimits,
) -> Result<ToolOutcomeLimits, ToolRuntimeError> {
    limits.validate()
}

pub fn enforce_outcome_limits(
    outcome: ToolOutcome,
    limits: ToolOutcomeLimits,
) -> Result<ToolOutcome, ToolRuntimeError> {
    outcome.enforce_limits(limits)
}

pub fn enforce_completed_output_contract(
    outcome: ToolOutcome,
    contract: &plakous_model::ToolOutputContract,
) -> ToolOutcome {
    outcome.enforce_completed_output_contract(contract)
}

pub fn runtime_outcome_unknown() -> ToolOutcome {
    ToolOutcome::runtime_outcome_unknown()
}

pub fn executor(
    registry: &FrozenToolRegistry,
    invocation: &ToolInvocation,
) -> Result<Arc<dyn ToolExecutor>, ToolRuntimeError> {
    registry.executor(invocation)
}

pub fn bind_effect_dispatch(
    invocation: ToolInvocation,
    intent_digest: Digest,
    claim_evidence_digest: Digest,
) -> ToolInvocation {
    invocation.with_effect_dispatch(intent_digest, claim_evidence_digest)
}

#[allow(
    clippy::too_many_arguments,
    reason = "the context authority is explicit"
)]
pub fn tool_context(
    run_id: RunId,
    turn_id: TurnId,
    attempt_id: AttemptId,
    cancellation: CancellationToken,
    deadline: Option<Timestamp>,
    clock: Arc<dyn Clock>,
    environment: Arc<ExecutionEnvironment>,
    authorization_grant: EffectAuthorizationGrant,
    progress: ProgressSink,
) -> ToolContext {
    ToolContext::authorized(
        run_id,
        turn_id,
        attempt_id,
        cancellation,
        deadline,
        clock,
        environment,
        authorization_grant,
        progress,
    )
}

// ---------------------------------------------------------------------------
// Projection construction for the composition crates.
// ---------------------------------------------------------------------------
//
// A composition presents one or two outer tools and runs the pack tools inside
// a cell, so it builds its own projections instead of taking a compiled menu.
// `ToolProjectionMapV2` retains executable authority, so its constructors are
// not part of the supported inventory; they are here, on the same unsupported
// tier as the agent's dispatch seam, for `plakous-compose`.

use crate::{
    CanonicalProcessOperationV2, PortableToolSchemaV2, ProviderModelProjectionIdentityV2,
    RegisteredToolIdentity, ToolProjectionExposureV2, ToolProjectionLoweringV2,
    ToolProjectionMapV2, ToolProjectionRaisingV2,
};
use plakous_model::{ProjectedCustomToolFormatV2, SemanticJsonV1, Utf8TextContractV1};

pub fn projection_map(
    registry: FrozenToolRegistry,
    target: ProviderModelProjectionIdentityV2,
    entries: impl IntoIterator<Item = ToolProjectionExposureV2>,
) -> Result<ToolProjectionMapV2, ToolRuntimeError> {
    ToolProjectionMapV2::new(registry, target, entries)
}

#[allow(
    clippy::too_many_arguments,
    reason = "one pass-through of the exposure constructor"
)]
pub fn projection_exposure(
    canonical_operations: impl IntoIterator<
        Item = (CanonicalProcessOperationV2, RegisteredToolIdentity),
    >,
    exposed_name: impl Into<String>,
    description: impl Into<String>,
    input_schema: PortableToolSchemaV2,
    defaults: SemanticJsonV1,
    lowering: ToolProjectionLoweringV2,
    output_schema: PortableToolSchemaV2,
    raising: ToolProjectionRaisingV2,
    target: ProviderModelProjectionIdentityV2,
) -> Result<ToolProjectionExposureV2, ToolRuntimeError> {
    ToolProjectionExposureV2::new(
        canonical_operations,
        exposed_name,
        description,
        input_schema,
        defaults,
        lowering,
        output_schema,
        raising,
        target,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "one pass-through of the custom exposure constructor"
)]
pub fn projection_exposure_custom(
    canonical_operations: impl IntoIterator<
        Item = (CanonicalProcessOperationV2, RegisteredToolIdentity),
    >,
    exposed_name: impl Into<String>,
    description: impl Into<String>,
    input: Utf8TextContractV1,
    format: ProjectedCustomToolFormatV2,
    defaults: SemanticJsonV1,
    lowering: ToolProjectionLoweringV2,
    output_schema: PortableToolSchemaV2,
    raising: ToolProjectionRaisingV2,
    target: ProviderModelProjectionIdentityV2,
) -> Result<ToolProjectionExposureV2, ToolRuntimeError> {
    ToolProjectionExposureV2::new_custom(
        canonical_operations,
        exposed_name,
        description,
        input,
        format,
        defaults,
        lowering,
        output_schema,
        raising,
        target,
    )
}

/// Neutral operation labels supplied by the process adapter, not inferred by L0.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessOperationKind {
    Start,
    Poll,
    WriteWait,
    Await,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessHintSource {
    Default,
    ModelHint,
    Background,
}

/// Content-free process observation, independent of retained tool text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessTelemetry {
    Promoted {
        operation: ProcessOperationKind,
        hint_source: ProcessHintSource,
        hint_ms: u64,
        preempt: crate::ToolPreempt,
    },
    Waited {
        operation: ProcessOperationKind,
        yielded: bool,
    },
}

pub fn with_process_telemetry(
    mut outcome: ToolOutcome,
    observation: ProcessTelemetry,
) -> ToolOutcome {
    outcome.process_telemetry = Some(observation);
    outcome
}

pub fn process_telemetry(outcome: &ToolOutcome) -> Option<ProcessTelemetry> {
    outcome.process_telemetry
}
