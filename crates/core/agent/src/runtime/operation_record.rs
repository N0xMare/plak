//! Immutable operation evidence, lifecycle folding and tool bindings.

use crate::runtime::continuation::runtime_component;
use crate::runtime::driver::DurableEffectIntentEnvelope;
use crate::runtime::terminal::DurableAgentError;
use crate::runtime::{
    DurableActorCommandEffect, DurableCommandEnvelope, OperationId, ToolExecutionResult,
    TurnRecoveryOperationId,
};
use crate::{
    AgentError, AgentErrorKind, DurableCommandDisposition, DurableCommandRecord,
    DurableOperationKind, DurableOperationRecord, DurableOperationState, ReconstructiveComponent,
    RecoveryAction, RecoveryHandlerSignature, RecoveryObligation, RecoveryObligationKind,
    RuntimeCheckpoint, RuntimeComponentKind, RuntimeLifecycle, RuntimeOperationLedger,
    RuntimeWorkingRoot, WorkingRecoveryPhase,
};
use plakous_model::{
    AttemptId, ContentBlock, ConversationView, Digest, DigestDomain, EntryId, ModelError,
    ModelErrorKind, ModelOperationRoot, ModelResponse, ToolCallBlock, ToolCallState,
    ToolExecutionOwner, TurnId,
};
use plakous_tools::ToolInvocation;
use std::collections::BTreeSet;

mod provider_outcome;
#[cfg(test)]
pub(in crate::runtime) use provider_outcome::test_support;
pub(in crate::runtime) use provider_outcome::{
    DecidedRemedy, FailTurnShape, ProviderAttemptResult, ProviderOutcome, RoundDecision,
    TurnBudgets, provider_outcome, recorded_provider_response,
};

impl TurnRecoveryOperationId {
    /// Returns the durable operation sequence within its session runtime.
    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.0
    }
}

impl From<OperationId> for TurnRecoveryOperationId {
    fn from(value: OperationId) -> Self {
        Self(value.0)
    }
}

#[cfg(test)]
pub(super) fn operation_binding_component(
    binding: Digest,
) -> Result<ReconstructiveComponent, AgentError> {
    runtime_component(
        RuntimeComponentKind::Operation,
        [
            b"plakous-agent/operation-binding-v1".as_slice(),
            binding.bytes(),
        ]
        .concat(),
    )
}

pub(super) fn operation_result_component<T: serde::Serialize>(
    result: &Result<T, AgentError>,
) -> Result<ReconstructiveComponent, AgentError> {
    let bytes = match result {
        Ok(value) => serde_json::to_vec(&("completed", value)).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "operation outcome could not be canonicalized",
            )
        })?,
        Err(error) => serde_json::to_vec(&("failed", DurableAgentError::from_error(error)?))
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "operation failure could not be canonicalized",
                )
            })?,
    };
    runtime_component(RuntimeComponentKind::Operation, bytes)
}

/// The terminal state of a round's provider operation carrying `outcome`.
pub(super) fn provider_terminal_state(
    attempt: Result<&ModelResponse, &AgentError>,
    outcome: &ProviderOutcome,
    dispatched: bool,
    expected_provider_dispatch: Option<(Digest, AttemptId)>,
) -> Result<DurableOperationState, AgentError> {
    let evidence = outcome.component()?;
    Ok(if !dispatched {
        DurableOperationState::ProvenNotDispatched(evidence)
    } else if let Some((prepared_call, attempt_id)) = expected_provider_dispatch
        && attempt.is_err_and(|error| {
            error
                .model_error()
                .and_then(ModelError::dispatch_rejection)
                .is_some_and(|proof| proof.matches(prepared_call, attempt_id))
        })
    {
        DurableOperationState::ProvenNotDispatched(evidence)
    } else if expected_provider_dispatch.is_some()
        && attempt.is_err_and(|error| {
            error
                .model_error()
                .and_then(ModelError::dispatch_rejection)
                .is_some()
        })
    {
        // A rejection claim bound to another call/attempt is not evidence for
        // this durable operation and cannot be downgraded to an ordinary
        // terminal provider failure.
        DurableOperationState::OutcomeUnknown(evidence)
    } else if attempt.is_err_and(|error| error.kind() == AgentErrorKind::OutcomeUnknown) {
        DurableOperationState::OutcomeUnknown(evidence)
    } else {
        DurableOperationState::Completed(evidence)
    })
}

pub(super) fn completed_tool_outcome_component(
    result: &Result<ToolExecutionResult, AgentError>,
    result_entry_id: EntryId,
) -> Result<ReconstructiveComponent, AgentError> {
    let bytes = match result {
        Ok(execution) => {
            serde_json::to_vec(&("completed-tool-outcome-v1", result_entry_id, execution))
        }
        Err(error) => serde_json::to_vec(&(
            "failed-tool-outcome-v1",
            result_entry_id,
            DurableAgentError::from_error(error)?,
        )),
    }
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Internal,
            "completed tool outcome could not be canonicalized",
        )
    })?;
    runtime_component(RuntimeComponentKind::Operation, bytes)
}

pub(super) fn tool_operation_terminal_state(
    result: &Result<ToolExecutionResult, AgentError>,
    dispatched: bool,
    result_entry_id: EntryId,
) -> Result<DurableOperationState, AgentError> {
    let evidence = completed_tool_outcome_component(result, result_entry_id)?;
    Ok(if !dispatched {
        DurableOperationState::ProvenNotDispatched(evidence)
    } else if result
        .as_ref()
        .is_err_and(|error| error.kind() == AgentErrorKind::OutcomeUnknown)
    {
        DurableOperationState::OutcomeUnknown(evidence)
    } else {
        DurableOperationState::Completed(evidence)
    })
}

pub(super) fn working_lifecycle(
    turn_id: TurnId,
    ledger: &RuntimeOperationLedger,
) -> Result<RuntimeLifecycle, AgentError> {
    working_lifecycle_for_root(ModelOperationRoot::Turn(turn_id), ledger)
}

pub(super) fn compaction_success_lifecycle(
    ledger: &RuntimeOperationLedger,
) -> Result<RuntimeLifecycle, AgentError> {
    let turn_id =
        ledger
            .operations()
            .into_iter()
            .find_map(|record| match (record.kind(), record.state()) {
                (
                    DurableOperationKind::Command {
                        turn_id: Some(turn_id),
                    },
                    DurableOperationState::Prepared | DurableOperationState::Dispatched,
                ) => Some(*turn_id),
                _ => None,
            });
    match turn_id {
        Some(turn_id) => working_lifecycle(turn_id, ledger),
        None => Ok(RuntimeLifecycle::Quiescent),
    }
}

pub(super) const fn compaction_receipt_lifecycle_ok(
    source: &RuntimeLifecycle,
    target: &RuntimeLifecycle,
    successful: bool,
) -> bool {
    matches!(
        (source, target, successful),
        (RuntimeLifecycle::Quiescent, RuntimeLifecycle::Quiescent, _)
            | (
                RuntimeLifecycle::Working { .. },
                RuntimeLifecycle::Working { .. },
                _
            )
            | (
                RuntimeLifecycle::Working { .. },
                RuntimeLifecycle::Quiescent,
                false
            )
    )
}

/// Prefer the turn root when a Dispatched Command shares the ledger with a
/// compaction `SessionWorkflow` (tool-group boundary). Idle compact remains
/// `SessionWorkflow`-only.
fn coalesce_working_roots(
    current: ModelOperationRoot,
    incoming: ModelOperationRoot,
) -> Result<ModelOperationRoot, AgentError> {
    if current == incoming {
        return Ok(current);
    }
    match (current, incoming) {
        (ModelOperationRoot::Turn(turn_id), ModelOperationRoot::SessionWorkflow(_))
        | (ModelOperationRoot::SessionWorkflow(_), ModelOperationRoot::Turn(turn_id)) => {
            Ok(ModelOperationRoot::Turn(turn_id))
        }
        _ => Err(AgentError::new(
            AgentErrorKind::Internal,
            "durable working set mixes logical operation roots",
        )),
    }
}

pub(super) fn working_lifecycle_for_root(
    default_root: ModelOperationRoot,
    ledger: &RuntimeOperationLedger,
) -> Result<RuntimeLifecycle, AgentError> {
    let mut active_root = None;
    for record in ledger
        .operations()
        .into_iter()
        .filter(|record| !matches!(record.state(), DurableOperationState::Finalized(_)))
    {
        if matches!(
            (record.kind(), record.state()),
            (
                DurableOperationKind::Command { .. },
                DurableOperationState::Prepared
            )
        ) {
            continue;
        }
        let Some(record_root) = operation_root(record) else {
            continue;
        };
        active_root = Some(match active_root {
            Some(existing) => coalesce_working_roots(existing, record_root)?,
            None => record_root,
        });
    }
    let operation_root = active_root.unwrap_or(default_root);
    let mut obligations = Vec::new();
    let mut phase = WorkingRecoveryPhase::Finalization;
    for record in ledger.operations() {
        let (kind, action) = match (record.kind(), record.state()) {
            (_, DurableOperationState::Finalized(_)) => continue,
            (
                DurableOperationKind::Provider { .. },
                DurableOperationState::Prepared
                | DurableOperationState::Dispatched
                | DurableOperationState::LocatorBound(_)
                | DurableOperationState::OutcomeUnknown(_),
            ) => (
                RecoveryObligationKind::ProviderAttempt,
                RecoveryAction::ProviderEvidenceRequired,
            ),
            (
                DurableOperationKind::Compaction { .. },
                DurableOperationState::Prepared
                | DurableOperationState::Dispatched
                | DurableOperationState::LocatorBound(_)
                | DurableOperationState::OutcomeUnknown(_),
            ) => (
                RecoveryObligationKind::Compaction,
                RecoveryAction::ProviderEvidenceRequired,
            ),
            (
                DurableOperationKind::Effect { .. },
                DurableOperationState::Prepared
                | DurableOperationState::Claimed(_)
                | DurableOperationState::Dispatched
                | DurableOperationState::OutcomeUnknown(_),
            ) => (
                RecoveryObligationKind::Effect,
                RecoveryAction::EffectEvidenceRequired,
            ),
            (
                DurableOperationKind::ProcessSession { .. },
                DurableOperationState::Prepared | DurableOperationState::Dispatched,
            ) => (
                RecoveryObligationKind::ProcessSession,
                RecoveryAction::EffectEvidenceRequired,
            ),
            (
                DurableOperationKind::CompositionEvaluation { .. }
                | DurableOperationKind::CompositionContinuation { .. }
                | DurableOperationKind::CompositionChild { .. },
                DurableOperationState::Dispatched
                | DurableOperationState::LocatorBound(_)
                | DurableOperationState::OutcomeUnknown(_),
            ) => (
                RecoveryObligationKind::CodeCell,
                RecoveryAction::EvaluationAbandonmentRequired,
            ),
            (
                DurableOperationKind::CompositionEvaluation { .. }
                | DurableOperationKind::CompositionContinuation { .. }
                | DurableOperationKind::CompositionChild { .. },
                DurableOperationState::Prepared,
            ) => (
                RecoveryObligationKind::CodeCell,
                RecoveryAction::ProvenNotDispatched,
            ),
            (DurableOperationKind::Commit { .. }, _) => (
                RecoveryObligationKind::Commit,
                RecoveryAction::CommitRecovery,
            ),
            (DurableOperationKind::Command { .. }, DurableOperationState::Prepared) => (
                RecoveryObligationKind::Command,
                RecoveryAction::ProvenNotDispatched,
            ),
            (DurableOperationKind::Command { .. }, DurableOperationState::Dispatched) => (
                RecoveryObligationKind::Command,
                RecoveryAction::FinalizationRequired,
            ),
            (
                DurableOperationKind::EffectDecision { .. },
                DurableOperationState::DecisionRequired(_),
            ) => (
                RecoveryObligationKind::EffectDecision,
                RecoveryAction::RestoreEffectDecision,
            ),
            (
                DurableOperationKind::ToolBatchContinuation { .. },
                DurableOperationState::ContinuationRequired(_),
            ) => (
                RecoveryObligationKind::HostedContinuation,
                RecoveryAction::RestoreToolBatch,
            ),
            (
                DurableOperationKind::ServerPauseContinuation { .. },
                DurableOperationState::ContinuationRequired(_),
            ) => (
                RecoveryObligationKind::HostedContinuation,
                RecoveryAction::RestoreServerPause,
            ),
            (DurableOperationKind::EffectDecision { .. }, DurableOperationState::Prepared) => (
                RecoveryObligationKind::EffectDecision,
                RecoveryAction::ProvenNotDispatched,
            ),
            (
                kind,
                DurableOperationState::Completed(_) | DurableOperationState::ProvenNotDispatched(_),
            ) => (kind_obligation(kind), RecoveryAction::FinalizationRequired),
            (_, DurableOperationState::OutcomeUnknown(_)) => {
                return Err(AgentError::new(
                    AgentErrorKind::Internal,
                    "operation state has no certified recovery action",
                ));
            }
            (_, DurableOperationState::LocatorBound(_)) => {
                return Err(AgentError::new(
                    AgentErrorKind::Internal,
                    "operation kind cannot bind provider recovery locator evidence",
                ));
            }
            (_, DurableOperationState::Claimed(_)) => {
                return Err(AgentError::new(
                    AgentErrorKind::Internal,
                    "non-effect operation retained a tool dispatch claim",
                ));
            }
            _ => {
                return Err(AgentError::new(
                    AgentErrorKind::Internal,
                    "operation kind retained an impossible durable state",
                ));
            }
        };
        phase = match action {
            RecoveryAction::ProvenNotDispatched if kind == RecoveryObligationKind::CodeCell => {
                WorkingRecoveryPhase::EffectReconciliation
            }
            RecoveryAction::RestoreEffectDecision
                if phase != WorkingRecoveryPhase::EffectReconciliation =>
            {
                WorkingRecoveryPhase::EffectDecision
            }
            RecoveryAction::RestoreToolBatch | RecoveryAction::RestoreServerPause => {
                if phase == WorkingRecoveryPhase::Finalization {
                    WorkingRecoveryPhase::HostedContinuation
                } else {
                    phase
                }
            }
            RecoveryAction::CommitRecovery => WorkingRecoveryPhase::CommitRecovery,
            RecoveryAction::EffectEvidenceRequired
            | RecoveryAction::EvaluationAbandonmentRequired => {
                WorkingRecoveryPhase::EffectReconciliation
            }
            RecoveryAction::ProviderEvidenceRequired | RecoveryAction::ProvenNotDispatched
                if !matches!(
                    phase,
                    WorkingRecoveryPhase::EffectReconciliation
                        | WorkingRecoveryPhase::EffectDecision
                ) =>
            {
                WorkingRecoveryPhase::DispatchEvidence
            }
            RecoveryAction::RestoreEffectDecision
            | RecoveryAction::ProviderEvidenceRequired
            | RecoveryAction::ProvenNotDispatched
            | RecoveryAction::FinalizationRequired => phase,
        };
        let signature = RecoveryHandlerSignature::new(kind, action);
        obligations.push(RecoveryObligation::new(record.operation_id(), signature));
    }
    if obligations.is_empty() {
        Ok(RuntimeLifecycle::Quiescent)
    } else {
        let root = match operation_root {
            ModelOperationRoot::Turn(turn_id) => RuntimeWorkingRoot::Turn {
                turn_id,
                next_round: 1,
            },
            ModelOperationRoot::SessionWorkflow(workflow_id) => {
                RuntimeWorkingRoot::SessionWorkflow(workflow_id)
            }
        };
        Ok(RuntimeLifecycle::Working {
            root,
            recovery_phase: phase,
            obligations,
        })
    }
}

fn command_operation_turn_id(record: &DurableOperationRecord) -> Option<TurnId> {
    if !matches!(record.kind(), DurableOperationKind::Command { .. }) {
        return None;
    }
    serde_json::from_slice::<(String, u64, TurnId, Digest, String, bool)>(record.request().bytes())
        .ok()
        .map(|decoded| decoded.2)
}

/// Records a quiescent ledger holds before a checkpoint drops finalized
/// evidence. Below this, keeping everything is cheaper than rebuilding.
pub(super) const LEDGER_RETENTION_HIGH_WATER: usize = 2_048;

/// Commands kept for idempotent retry, oldest first to go.
const COMMAND_RETENTION_WINDOW: usize = 256;

/// Whether the ledger has grown enough to drop finalized evidence.
pub(super) fn ledger_needs_retention(ledger: &RuntimeOperationLedger) -> bool {
    ledger
        .command_count()
        .saturating_add(ledger.operation_count())
        >= LEDGER_RETENTION_HIGH_WATER
}

/// Drops the finalized evidence of earlier turns from a quiescent ledger.
///
/// Kept: every operation that is not `Finalized`; the last
/// [`COMMAND_RETENTION_WINDOW`] commands (the idempotency window), commands
/// for still-owned or unsettled roots and, for
/// each accepted root among them, its root operation, which is what a reopen
/// rebuilds that turn's result from; the last accepted root turn and
/// everything after its root operation; the latest completed provider
/// response, which carries the
/// previous-response metadata of the next request; and every composition group
/// any kept record belongs to, since a group is only valid whole.
pub(super) fn retain_quiescent_ledger(
    ledger: &RuntimeOperationLedger,
    owned_turns: &BTreeSet<TurnId>,
) -> Result<RuntimeOperationLedger, AgentError> {
    let commands = ledger.commands();
    let window_start = commands.len().saturating_sub(COMMAND_RETENTION_WINDOW);
    let last_root = commands
        .iter()
        .rposition(|command| is_accepted_root(command));
    let operations = ledger.operations();
    let mut unsettled_turns = operations
        .iter()
        .filter_map(|record| {
            if matches!(record.state(), DurableOperationState::Finalized(_)) {
                return None;
            }
            match record.kind() {
                DurableOperationKind::Command { turn_id } => *turn_id,
                _ => None,
            }
        })
        .collect::<BTreeSet<_>>();
    unsettled_turns.extend(owned_turns.iter().copied());
    let first_sequence = commands
        .get(window_start)
        .map_or(u64::MAX, |command| command.sequence());
    let kept_roots = commands
        .iter()
        .filter(|command| {
            command.sequence() >= first_sequence
                || command
                    .turn_id()
                    .is_some_and(|turn_id| unsettled_turns.contains(&turn_id))
        })
        .filter(|command| is_accepted_root(command))
        .filter_map(|command| command.turn_id())
        .collect::<BTreeSet<_>>();
    let last_turn = last_root.and_then(|index| commands[index].turn_id());
    let root_of = |record: &DurableOperationRecord| match record.kind() {
        DurableOperationKind::Command {
            turn_id: Some(turn_id),
        } => Some(*turn_id),
        _ => None,
    };
    let last_turn_start = operations
        .iter()
        .find(|record| last_turn.is_some() && root_of(record) == last_turn)
        .map_or(u64::MAX, |record| record.operation_id());
    let latest_response = operations
        .iter()
        .rev()
        .find(|record| completed_provider_response(record))
        .map(|record| record.operation_id());
    let base = |record: &DurableOperationRecord| {
        !matches!(record.state(), DurableOperationState::Finalized(_))
            || record.operation_id() >= last_turn_start
            || Some(record.operation_id()) == latest_response
            || root_of(record).is_some_and(|turn_id| kept_roots.contains(&turn_id))
    };
    let groups = operations
        .iter()
        .filter(|record| base(record))
        .filter_map(|record| composition_group(record))
        .collect::<BTreeSet<_>>();
    ledger
        .retained(
            |command| {
                command.sequence() >= first_sequence
                    || command
                        .turn_id()
                        .is_some_and(|turn_id| unsettled_turns.contains(&turn_id))
                    || last_root
                        .is_some_and(|index| command.sequence() == commands[index].sequence())
            },
            |record| {
                base(record)
                    || composition_group(record).is_some_and(|group| groups.contains(&group))
            },
        )
        .map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "operation ledger retention produced an invalid ledger",
            )
        })
}

/// The evaluation a composition record belongs to, named by its operation.
fn composition_group(record: &DurableOperationRecord) -> Option<u64> {
    match record.kind() {
        DurableOperationKind::CompositionEvaluation { .. } => Some(record.operation_id()),
        DurableOperationKind::CompositionContinuation {
            evaluation_operation_id,
            ..
        }
        | DurableOperationKind::CompositionChild {
            evaluation_operation_id,
            ..
        } => Some(*evaluation_operation_id),
        _ => None,
    }
}

fn completed_provider_response(record: &DurableOperationRecord) -> bool {
    matches!(
        record.state(),
        DurableOperationState::Completed(_) | DurableOperationState::Finalized(_)
    ) && recorded_provider_response(record)
        // A corrupt record is retained for recovery to refuse, never pruned.
        .map_or(true, |response| {
            response.is_some_and(|response| !response.output().is_empty())
        })
}

/// Whether a command admitted a root turn.
pub(in crate::runtime) fn is_accepted_root(command: &DurableCommandRecord) -> bool {
    command.disposition() == DurableCommandDisposition::Accepted
        && serde_json::from_slice::<DurableCommandEnvelope>(command.command_component().bytes())
            .is_ok_and(|envelope| matches!(envelope.effect, DurableActorCommandEffect::Root))
}

pub(super) fn operation_turn_id(record: &DurableOperationRecord) -> Option<TurnId> {
    operation_root(record).and_then(ModelOperationRoot::turn_id)
}

fn operation_root(record: &DurableOperationRecord) -> Option<ModelOperationRoot> {
    match record.kind() {
        DurableOperationKind::Command {
            turn_id: Some(turn_id),
        }
        | DurableOperationKind::EffectDecision { turn_id, .. }
        | DurableOperationKind::Effect { turn_id, .. }
        | DurableOperationKind::ToolBatchContinuation { turn_id, .. }
        | DurableOperationKind::ServerPauseContinuation { turn_id, .. }
        | DurableOperationKind::CompositionEvaluation { turn_id, .. }
        | DurableOperationKind::CompositionContinuation { turn_id, .. }
        | DurableOperationKind::CompositionChild { turn_id, .. }
        | DurableOperationKind::ProcessSession { turn_id, .. } => {
            Some(ModelOperationRoot::Turn(*turn_id))
        }
        DurableOperationKind::Provider { root, .. } | DurableOperationKind::Commit { root } => {
            Some(*root)
        }
        DurableOperationKind::Compaction { workflow_id } => {
            Some(ModelOperationRoot::SessionWorkflow(*workflow_id))
        }
        DurableOperationKind::Command { turn_id: None } => {
            command_operation_turn_id(record).map(ModelOperationRoot::Turn)
        }
    }
}

pub(super) const fn operation_kind_turn_id(kind: &DurableOperationKind) -> Option<TurnId> {
    match kind {
        DurableOperationKind::Command { turn_id } => *turn_id,
        DurableOperationKind::Provider { root, .. } | DurableOperationKind::Commit { root } => {
            root.turn_id()
        }
        DurableOperationKind::EffectDecision { turn_id, .. }
        | DurableOperationKind::Effect { turn_id, .. }
        | DurableOperationKind::ToolBatchContinuation { turn_id, .. }
        | DurableOperationKind::ServerPauseContinuation { turn_id, .. }
        | DurableOperationKind::CompositionEvaluation { turn_id, .. }
        | DurableOperationKind::CompositionContinuation { turn_id, .. }
        | DurableOperationKind::CompositionChild { turn_id, .. }
        | DurableOperationKind::ProcessSession { turn_id, .. } => Some(*turn_id),
        DurableOperationKind::Compaction { .. } => None,
    }
}

pub(super) fn effect_operation_intent_digest(
    checkpoint: &RuntimeCheckpoint,
    operation_id: OperationId,
) -> Result<Digest, AgentError> {
    let record = checkpoint
        .manifest()
        .ledger()
        .operation(operation_id.0)
        .ok_or_else(|| {
            AgentError::new(
                AgentErrorKind::Internal,
                "effect claim referenced an unknown operation",
            )
        })?;
    let envelope = serde_json::from_slice::<DurableEffectIntentEnvelope>(record.request().bytes())
        .map_err(|_| {
            AgentError::new(
                AgentErrorKind::Session,
                "durable effect intent could not be reconstructed",
            )
        })?;
    Ok(envelope.intent_digest)
}

const fn kind_obligation(kind: &DurableOperationKind) -> RecoveryObligationKind {
    match kind {
        DurableOperationKind::Command { .. } => RecoveryObligationKind::Command,
        DurableOperationKind::Provider { .. } => RecoveryObligationKind::ProviderAttempt,
        DurableOperationKind::EffectDecision { .. } => RecoveryObligationKind::EffectDecision,
        DurableOperationKind::Effect { .. } => RecoveryObligationKind::Effect,
        DurableOperationKind::ToolBatchContinuation { .. }
        | DurableOperationKind::ServerPauseContinuation { .. } => {
            RecoveryObligationKind::HostedContinuation
        }
        DurableOperationKind::CompositionEvaluation { .. }
        | DurableOperationKind::CompositionContinuation { .. }
        | DurableOperationKind::CompositionChild { .. } => RecoveryObligationKind::CodeCell,
        DurableOperationKind::ProcessSession { .. } => RecoveryObligationKind::ProcessSession,
        DurableOperationKind::Commit { .. } => RecoveryObligationKind::Commit,
        DurableOperationKind::Compaction { .. } => RecoveryObligationKind::Compaction,
    }
}

pub(super) fn tool_operation_binding(
    turn_id: TurnId,
    attempt_id: AttemptId,
    source_index: usize,
    call: &ToolCallBlock,
) -> Result<Digest, AgentError> {
    let call_bytes = serde_json::to_vec(call).map_err(|_| {
        AgentError::new(
            AgentErrorKind::Internal,
            "tool operation input could not be canonicalized",
        )
    })?;
    Digest::sha256(
        DigestDomain::ProviderOperation,
        2,
        &[
            b"tool",
            turn_id.as_uuid().as_bytes(),
            attempt_id.as_uuid().as_bytes(),
            &source_index.to_be_bytes(),
            &call_bytes,
        ],
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Internal,
            "tool operation identity could not be constructed",
        )
    })
}

pub(super) fn canonical_tool_operation_binding(
    source_index: usize,
    invocation: &ToolInvocation,
) -> Result<Digest, AgentError> {
    let identity = invocation.identity();
    let canonical = serde_json::to_vec(&(
        identity.tool_id().as_str(),
        identity.name(),
        identity.semantic_version(),
        identity.definition_digest(),
        identity.executor_id().as_str(),
        invocation.call_id(),
        invocation.correlation(),
        invocation.arguments().digest(),
    ))
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Internal,
            "canonical tool operation input could not be encoded",
        )
    })?;
    Digest::sha256(
        DigestDomain::ProviderOperation,
        3,
        &[
            b"canonical-tool",
            invocation.turn_id().as_uuid().as_bytes(),
            invocation.attempt_id().as_uuid().as_bytes(),
            &source_index.to_be_bytes(),
            &canonical,
        ],
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Internal,
            "canonical tool operation identity could not be constructed",
        )
    })
}

pub(super) fn collect_local_calls<'a>(
    response: &'a ModelResponse,
    history: &ConversationView,
) -> Result<Vec<&'a ToolCallBlock>, ModelError> {
    let mut calls = Vec::new();
    let mut identities = BTreeSet::new();
    for item in history.items() {
        for block in item.blocks() {
            if let ContentBlock::ToolCall(call) = block {
                identities.insert(call.call_id());
            }
        }
    }
    for item in response.output() {
        for block in item.blocks() {
            let ContentBlock::ToolCall(call) = block else {
                continue;
            };
            if !identities.insert(call.call_id()) {
                return Err(ModelError::new(
                    ModelErrorKind::InvalidEvent,
                    "model response contains a duplicate tool-call identity",
                ));
            }
            match call.owner() {
                ToolExecutionOwner::PlakLocal | ToolExecutionOwner::PlakMcp { .. }
                    if matches!(
                        call.state(),
                        ToolCallState::Requested | ToolCallState::Pending
                    ) =>
                {
                    calls.push(call);
                }
                ToolExecutionOwner::PlakLocal | ToolExecutionOwner::PlakMcp { .. } => {
                    return Err(ModelError::new(
                        ModelErrorKind::InvalidEvent,
                        "model requested local dispatch with a terminal tool-call state",
                    ));
                }
                ToolExecutionOwner::ProviderHosted { .. } => {}
                _ => {
                    return Err(ModelError::new(
                        ModelErrorKind::InvalidEvent,
                        "model response contains an unsupported tool execution owner",
                    ));
                }
            }
        }
    }
    Ok(calls)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::control::{command_binding, command_canonical_bytes, durable_rejection};
    use crate::{CommandId, ControlError, ControlErrorKind, TurnCommand};

    #[test]
    fn retention_keeps_owned_roots_outside_the_settled_command_window()
    -> Result<(), Box<dyn std::error::Error>> {
        let owned = TurnId::try_from(uuid::Uuid::from_u128(1))?;
        let latest = TurnId::try_from(uuid::Uuid::from_u128(2))?;
        let mut commands = Vec::new();
        for sequence in 1..=4_096 {
            let accepted = sequence <= 2;
            let command = TurnCommand::Start {
                command_id: CommandId::new(format!("retention:{sequence}"))?,
                text: if accepted {
                    "root".into()
                } else {
                    String::new()
                },
            };
            let envelope = DurableCommandEnvelope {
                version: 1,
                canonical_input: command_canonical_bytes(&command),
                effect: if accepted {
                    DurableActorCommandEffect::Root
                } else {
                    DurableActorCommandEffect::Rejected
                },
                rejection: (!accepted).then(|| {
                    durable_rejection(&ControlError::new(
                        ControlErrorKind::InvalidInput,
                        "empty prompt",
                    ))
                }),
            };
            commands.push(DurableCommandRecord::new(
                command.command_id().as_str(),
                sequence,
                match sequence {
                    1 => Some(owned),
                    2 => Some(latest),
                    _ => None,
                },
                command_binding(&command)?,
                runtime_component(
                    RuntimeComponentKind::Command,
                    serde_json::to_vec(&envelope)?,
                )?,
                if accepted {
                    DurableCommandDisposition::Accepted
                } else {
                    DurableCommandDisposition::Rejected
                },
                (!accepted).then(|| "InvalidInput".into()),
            )?);
        }
        let ledger = RuntimeOperationLedger::new(commands, Vec::new())?;
        let retained = retain_quiescent_ledger(&ledger, &BTreeSet::from([owned]))?;
        assert_eq!(retained.command_count(), COMMAND_RETENTION_WINDOW + 2);
        assert!(
            retained
                .commands()
                .iter()
                .any(|command| command.turn_id() == Some(owned))
        );
        assert!(
            retained
                .commands()
                .iter()
                .any(|command| command.turn_id() == Some(latest))
        );
        let settled = retain_quiescent_ledger(&ledger, &BTreeSet::new())?;
        assert_eq!(settled.command_count(), COMMAND_RETENTION_WINDOW + 1);
        assert!(
            !settled
                .commands()
                .iter()
                .any(|command| command.turn_id() == Some(owned))
        );
        Ok(())
    }
}
