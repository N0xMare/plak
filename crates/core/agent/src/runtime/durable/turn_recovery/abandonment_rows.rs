//! Turn abandonment by obligation kind: how each outstanding obligation folds
//! (spec 01 section 13), and how each folded debt closes.

use super::fold_rows::FoldRow;
use super::{
    abandoned_compaction_evidence, abandoned_effect_evidence, abandoned_operation_evidence,
    abandoned_process_evidence, abandoned_turn_segment, abandoned_turn_segment_with_note,
    close_as_unobserved, controls, finalize_not_started, model_round_boundary, replay,
    root_is_only_turn_operation, terminal_turn_usage, unsent_decision,
};
use crate::runtime::continuation::{runtime_component, runtime_manifest_error};
use crate::runtime::durable::composition_recovery::{code_cell_abandonment_fold, pending_steering};
use crate::runtime::durable::{AbandonedRoot, AbandonedTurnRecoveryFold};
use crate::runtime::operation_record::operation_turn_id;
use crate::runtime::terminal::{
    DurableAgentError, DurableTerminalOutcome, PendingTerminalResultRecord,
    TERMINAL_RESULT_RECORD_VERSION, encode_pending_terminal_record, finalized_operation_evidence,
};
use crate::runtime::{AbandonedTurnClaim, AbandonmentCause, OperationId};
use crate::{
    AgentError, AgentErrorKind, CheckpointId, DurableOperationKind, DurableOperationRecord,
    DurableOperationState, DurableOperationTransition, RecoveryAction, RecoveryObligation,
    RecoveryObligationKind, RuntimeCheckpoint, RuntimeComponentKind, RuntimeOperationLedger,
    SessionStoreError,
};
use plakous_model::{Digest, HistorySegment, TurnId};

/// Where one turn-owned obligation lands in an abandonment fold.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TurnRow {
    /// The turn's root command.
    Root,
    /// Completed root-command evidence a Code Cell turn keeps as it is.
    Retained,
    /// Completed or proven-unrun evidence that owes only finalization.
    Settled,
    /// A completed effect whose result never reached model history.
    CompletedUnadded,
    /// A provider attempt whose outcome nobody observed.
    ProviderUncertain,
    /// A tool effect that may have run.
    EffectUncertain,
    /// A tool effect proven never to have started.
    EffectUnrun,
    /// A process session that may have run.
    ProcessUncertain,
}

/// The facts about the abandoned turn every row reads.
pub(super) struct TurnFoldFacts<'a> {
    pub(super) ledger: &'a RuntimeOperationLedger,
    pub(super) turn_id: TurnId,
    /// A Code Cell fold owns this turn's abandonment.
    pub(super) code_cell: bool,
    pub(super) root_dispatched: bool,
    /// The root's last round completed with a tool batch whose results were
    /// never added.
    pub(super) batch_pending: bool,
}

/// The row of one turn-owned obligation, or `None` when a turn cannot abandon
/// it. `root_open` is whether no root command has been claimed yet.
pub(super) fn turn_row(
    facts: &TurnFoldFacts<'_>,
    obligation: &RecoveryObligation,
    record: &DurableOperationRecord,
    root_open: bool,
) -> Option<TurnRow> {
    let (action, state) = (obligation.action(), record.state());
    match obligation.kind() {
        RecoveryObligationKind::Command => command_row(facts, record, action, state, root_open),
        RecoveryObligationKind::CodeCell => code_cell_row(facts, action, state),
        RecoveryObligationKind::Effect => effect_row(facts, record, action, state),
        RecoveryObligationKind::ProviderAttempt => provider_row(action, state),
        RecoveryObligationKind::ProcessSession => process_row(action, state),
        RecoveryObligationKind::EffectDecision
        | RecoveryObligationKind::HostedContinuation
        | RecoveryObligationKind::Commit
        | RecoveryObligationKind::Compaction => None,
    }
}

fn command_row(
    facts: &TurnFoldFacts<'_>,
    record: &DurableOperationRecord,
    action: RecoveryAction,
    state: &DurableOperationState,
    root_open: bool,
) -> Option<TurnRow> {
    match (action, state) {
        (RecoveryAction::ProvenNotDispatched, DurableOperationState::Prepared)
            if root_open
                && (facts.code_cell
                    || root_is_only_turn_operation(
                        facts.ledger,
                        facts.turn_id,
                        OperationId(record.operation_id()),
                    )) =>
        {
            Some(TurnRow::Root)
        }
        (RecoveryAction::FinalizationRequired, DurableOperationState::Completed(_))
            if facts.code_cell =>
        {
            Some(TurnRow::Retained)
        }
        (RecoveryAction::FinalizationRequired, DurableOperationState::Dispatched) if root_open => {
            Some(TurnRow::Root)
        }
        _ => None,
    }
}

fn code_cell_row(
    facts: &TurnFoldFacts<'_>,
    action: RecoveryAction,
    state: &DurableOperationState,
) -> Option<TurnRow> {
    match (action, state) {
        (
            RecoveryAction::FinalizationRequired,
            DurableOperationState::Completed(_) | DurableOperationState::ProvenNotDispatched(_),
        ) if facts.code_cell => Some(TurnRow::Settled),
        (RecoveryAction::FinalizationRequired, DurableOperationState::Completed(_))
            if facts.batch_pending =>
        {
            Some(TurnRow::CompletedUnadded)
        }
        _ => None,
    }
}

fn effect_row(
    facts: &TurnFoldFacts<'_>,
    record: &DurableOperationRecord,
    action: RecoveryAction,
    state: &DurableOperationState,
) -> Option<TurnRow> {
    match (action, state) {
        (
            RecoveryAction::FinalizationRequired,
            DurableOperationState::Completed(_) | DurableOperationState::ProvenNotDispatched(_),
        ) if facts.code_cell => Some(TurnRow::Settled),
        (RecoveryAction::FinalizationRequired, DurableOperationState::Completed(_))
            if facts.root_dispatched && asking_round_completed(facts, record) =>
        {
            Some(TurnRow::CompletedUnadded)
        }
        (
            RecoveryAction::EffectEvidenceRequired,
            DurableOperationState::Prepared
            | DurableOperationState::Claimed(_)
            | DurableOperationState::Dispatched
            | DurableOperationState::OutcomeUnknown(_),
        ) => Some(TurnRow::EffectUncertain),
        // The one effect state that is evidence rather than silence. Closing
        // it as uncertain would put a warning about the workspace in front of
        // a user over a command this session can prove never started.
        (RecoveryAction::EffectEvidenceRequired, DurableOperationState::ProvenNotDispatched(_)) => {
            Some(TurnRow::EffectUnrun)
        }
        _ => None,
    }
}

/// Whether the turn's provider round that asked for this effect completed.
fn asking_round_completed(facts: &TurnFoldFacts<'_>, effect: &DurableOperationRecord) -> bool {
    let DurableOperationKind::Effect { attempt_id, .. } = effect.kind() else {
        return false;
    };
    facts.ledger.operations().into_iter().any(|provider| {
        matches!(provider.kind(),
            DurableOperationKind::Provider { attempt_id: provider_attempt, .. }
            if provider_attempt == attempt_id)
            && operation_turn_id(provider) == Some(facts.turn_id)
            && matches!(provider.state(), DurableOperationState::Completed(_))
    })
}

// Completed rounds and joined processes retain their exact evidence. Another
// obligation may still be unfinished, but these are not uncertain and cannot
// justify abandonment on their own.

fn provider_row(action: RecoveryAction, state: &DurableOperationState) -> Option<TurnRow> {
    match (action, state) {
        (
            RecoveryAction::ProviderEvidenceRequired,
            DurableOperationState::Prepared
            | DurableOperationState::Dispatched
            | DurableOperationState::LocatorBound(_)
            | DurableOperationState::OutcomeUnknown(_),
        ) => Some(TurnRow::ProviderUncertain),
        (RecoveryAction::FinalizationRequired, DurableOperationState::Completed(_)) => {
            Some(TurnRow::Settled)
        }
        _ => None,
    }
}

fn process_row(action: RecoveryAction, state: &DurableOperationState) -> Option<TurnRow> {
    match (action, state) {
        (RecoveryAction::FinalizationRequired, DurableOperationState::Completed(_)) => {
            Some(TurnRow::Settled)
        }
        (
            RecoveryAction::EffectEvidenceRequired,
            DurableOperationState::Prepared | DurableOperationState::Dispatched,
        ) => Some(TurnRow::ProcessUncertain),
        _ => None,
    }
}

/// What a closure knows about the abandonment it writes.
pub(super) struct ClosureFacts<'a> {
    pub(super) source: &'a RuntimeCheckpoint,
    pub(super) fold: &'a AbandonedTurnRecoveryFold,
    pub(super) claim: AbandonedTurnClaim,
    pub(super) cause: AbandonmentCause,
    /// A fold row that words its own note and cancellation.
    pub(super) row: Option<FoldRow>,
    /// The only outstanding tool work is completed effects whose results
    /// were never added.
    pub(super) completed_effect_only: bool,
}

/// The history segment that records the abandonment, worded for its
/// strongest claim.
pub(super) fn abandonment_segment(facts: &ClosureFacts<'_>) -> Result<HistorySegment, AgentError> {
    let ClosureFacts {
        source,
        fold,
        claim,
        cause,
        row,
        completed_effect_only,
    } = *facts;
    let turn_id = fold.turn_id;
    if let Some(note) = row.and_then(|row| row.note(cause)) {
        return abandoned_turn_segment_with_note(turn_id, note);
    }
    if claim == AbandonedTurnClaim::CompactionNotApplied
        && source
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .any(|record| {
                operation_turn_id(record) == Some(turn_id)
                    && replay::retained_round(record).is_ok_and(|retained| retained)
            })
    {
        return abandoned_turn_segment_with_note(
            turn_id,
            match cause {
                AbandonmentCause::ProcessLost => {
                    "this turn was abandoned after the process was lost while the conversation was being compacted. Compaction was not applied and may have been billed. Earlier completed rounds of this turn remain in this history; the history before this point is unchanged."
                }
                AbandonmentCause::Interrupted => {
                    "this turn stopped while the conversation was being compacted. Compaction was not applied and may have been billed. Earlier completed rounds of this turn remain in this history; the history before this point is unchanged."
                }
            },
        );
    }
    if let Some(unsent) = unsent_decision(source, turn_id, fold.root_operation) {
        return abandoned_turn_segment_with_note(
            turn_id,
            match unsent {
                replay::UnsentDecision::Retry => {
                    "this turn was abandoned after its model request failed and a retry was decided. The decided retry was not sent and is not retried; the history before this point is unchanged."
                }
                replay::UnsentDecision::Fold => {
                    "this turn was abandoned after the provider refused its request as over the context window. Compaction did not start, and the refused request was not sent again; the history before this point is unchanged."
                }
            },
        );
    }
    if fold.tool_round_completed && model_round_boundary(source, turn_id, fold.root_operation) {
        return abandoned_turn_segment_with_note(
            turn_id,
            "this turn was abandoned after its model round completed, before the next model request was prepared. The completed model response remains in this history; no next request was dispatched or retried.",
        );
    }
    if fold.tool_round_completed && claim == AbandonedTurnClaim::RunningTool {
        return abandoned_turn_segment_with_note(
            turn_id,
            "this turn was abandoned after its tool round completed, before the next model \
     request was prepared. Completed tool results remain in this history. A process \
     it started was still running; its outcome is unknown, it may have changed the \
     workspace, and it is not retried. Check the workspace before carrying on.",
        );
    }
    if claim == AbandonedTurnClaim::RunningTool && completed_effect_only {
        return abandoned_turn_segment_with_note(
            turn_id,
            "this turn was abandoned after a tool finished, but its result was not added \
             to the conversation. It may have changed the workspace and is not retried. \
             Check the workspace before relying on what came before this point.",
        );
    }
    if claim == AbandonedTurnClaim::NothingStarted
        && code_cell_abandonment_fold(source).is_ok_and(|cell| cell.turn_abandonment)
    {
        return abandoned_turn_segment_with_note(
            turn_id,
            "this turn's remaining Code Cell work was abandoned before dispatch. The abandoned \
     operations never started and are not retried; completed work retains its own \
     evidence. No cell result is inferred from this abandonment.",
        );
    }
    if claim == AbandonedTurnClaim::NothingStarted
        && root_is_only_turn_operation(source.manifest().ledger(), turn_id, fold.root_operation)
    {
        return abandoned_turn_segment_with_note(
            turn_id,
            "this turn was abandoned after its process was lost, before any model request \
     or tool started. Nothing ran, no result was inferred, and the prompt is not retried.",
        );
    }
    abandoned_turn_segment(turn_id, claim, cause)
}

/// Close every operation whose outcome nobody observed as unknown, each with
/// the evidence of its own obligation kind.
pub(super) fn close_uncertain_operations(
    mut ledger: RuntimeOperationLedger,
    fold: &AbandonedTurnRecoveryFold,
    checkpoint_id: CheckpointId,
) -> Result<RuntimeOperationLedger, AgentError> {
    let unknown = [
        (&fold.provider_operations, abandoned_operation_evidence()?),
        (&fold.effect_operations, abandoned_effect_evidence()?),
        (&fold.process_operations, abandoned_process_evidence()?),
        (
            &fold.compaction_operations,
            abandoned_compaction_evidence()?,
        ),
    ];
    for (operations, evidence) in unknown {
        for operation in operations {
            ledger = close_as_unobserved(&ledger, *operation, &evidence, checkpoint_id)?;
        }
    }
    Ok(ledger)
}

/// Finalize completed provider/process evidence and proven-unrun effects with
/// their own evidence. Rewriting them as unknown would lose facts; none
/// strengthens the note.
pub(super) fn finalize_retained_operations(
    mut ledger: RuntimeOperationLedger,
    source: &RuntimeCheckpoint,
    fold: &AbandonedTurnRecoveryFold,
    checkpoint_id: CheckpointId,
    history_identity: Digest,
) -> Result<RuntimeOperationLedger, AgentError> {
    for operation in fold
        .settled_operations
        .iter()
        .chain(&fold.unrun_effect_operations)
    {
        let record = ledger
            .operation(operation.0)
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
        if controls::same_turn_control(&ledger, record, fold.turn_id) {
            ledger = controls::settle_control(&ledger, record, checkpoint_id)?;
            continue;
        }
        if matches!(record.state(), DurableOperationState::Prepared) {
            ledger = finalize_not_started(&ledger, record, checkpoint_id, history_identity)?;
            continue;
        }
        if matches!(record.state(), DurableOperationState::Dispatched)
            && pending_steering(source, record, fold.turn_id)
        {
            ledger = ledger
                .transition_operation(
                    operation.0,
                    DurableOperationTransition::new(
                        checkpoint_id,
                        DurableOperationState::Completed(runtime_component(
                            RuntimeComponentKind::Operation,
                            controls::STEERING_NOT_APPLIED.to_vec(),
                        )?),
                    ),
                )
                .map_err(runtime_manifest_error)?;
        }
        let evidence = ledger
            .operation(operation.0)
            .and_then(|record| match record.state() {
                DurableOperationState::Completed(evidence)
                | DurableOperationState::ProvenNotDispatched(evidence) => Some(evidence.clone()),
                _ => None,
            })
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
        ledger = ledger
            .transition_operation(
                operation.0,
                DurableOperationTransition::new(
                    checkpoint_id,
                    DurableOperationState::Finalized(evidence),
                ),
            )
            .map_err(runtime_manifest_error)?;
    }
    Ok(ledger)
}

/// Close the turn's root command by its abandoned shape.
pub(super) fn close_root(
    ledger: RuntimeOperationLedger,
    facts: &ClosureFacts<'_>,
    checkpoint_id: CheckpointId,
    history_identity: Digest,
) -> Result<RuntimeOperationLedger, AgentError> {
    let fold = facts.fold;
    match fold.root {
        AbandonedRoot::Prepared => {
            let record = ledger
                .operation(fold.root_operation.0)
                .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
            finalize_not_started(&ledger, record, checkpoint_id, history_identity)
        }
        AbandonedRoot::Dispatched => {
            cancel_dispatched_root(&ledger, facts, checkpoint_id, history_identity)
        }
        AbandonedRoot::PendingTerminal => {
            let evidence = ledger
                .operation(fold.root_operation.0)
                .and_then(|record| match record.state() {
                    DurableOperationState::Completed(evidence) => Some(evidence),
                    _ => None,
                })
                .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
            let finalized =
                finalized_operation_evidence(evidence, checkpoint_id, history_identity)?;
            ledger
                .transition_operation(
                    fold.root_operation.0,
                    DurableOperationTransition::new(
                        checkpoint_id,
                        DurableOperationState::Finalized(finalized),
                    ),
                )
                .map_err(runtime_manifest_error)
        }
        AbandonedRoot::AlreadyTerminal => Ok(ledger),
    }
}

/// Finalize a dispatched root as cancelled: the turn's one terminal result.
fn cancel_dispatched_root(
    ledger: &RuntimeOperationLedger,
    facts: &ClosureFacts<'_>,
    checkpoint_id: CheckpointId,
    history_identity: Digest,
) -> Result<RuntimeOperationLedger, AgentError> {
    let fold = facts.fold;
    let cancelled = DurableAgentError::from_error(&AgentError::new(
        AgentErrorKind::Session,
        dispatched_root_cancellation(facts),
    ))?;
    let pending = encode_pending_terminal_record(PendingTerminalResultRecord {
        remedy: None,
        version: TERMINAL_RESULT_RECORD_VERSION,
        command_id: fold.command_id.as_str().to_owned(),
        turn_id: fold.turn_id,
        operation_id: fold.root_operation,
        outcome: DurableTerminalOutcome::Cancelled(cancelled),
        usage: terminal_turn_usage(&facts.source.manifest().ledger().operations(), fold.turn_id)?,
    })
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Internal,
            "abandoned terminal record could not be encoded",
        )
    })?;
    let finalized = finalized_operation_evidence(
        &runtime_component(RuntimeComponentKind::Operation, pending)?,
        checkpoint_id,
        history_identity,
    )?;
    ledger
        .transition_operation(
            fold.root_operation.0,
            DurableOperationTransition::new(
                checkpoint_id,
                DurableOperationState::Finalized(finalized),
            ),
        )
        .map_err(runtime_manifest_error)
}

/// The cancellation a dispatched root records, worded for its claim.
fn dispatched_root_cancellation(facts: &ClosureFacts<'_>) -> &'static str {
    let ClosureFacts {
        source,
        fold,
        claim,
        row,
        completed_effect_only,
        ..
    } = *facts;
    if let Some(cancellation) = row.and_then(FoldRow::cancellation) {
        return cancellation;
    }
    match claim {
        AbandonedTurnClaim::ModelRequest => {
            "the turn was abandoned after its model request's outcome became uncertain"
        }
        AbandonedTurnClaim::RunningTool if completed_effect_only => {
            "the turn was abandoned after a tool finished, but its result was not added to the conversation; it may have changed the workspace and is not retried"
        }
        AbandonedTurnClaim::RunningTool => {
            "the turn was abandoned after the process was lost while a tool was running"
        }
        AbandonedTurnClaim::CompactionNotApplied => {
            "the turn was abandoned after the process was lost during compaction"
        }
        AbandonedTurnClaim::NothingStarted
            if unsent_decision(source, fold.turn_id, fold.root_operation)
                == Some(replay::UnsentDecision::Retry) =>
        {
            "the turn was abandoned after a decided retry was not sent"
        }
        AbandonedTurnClaim::NothingStarted
            if unsent_decision(source, fold.turn_id, fold.root_operation)
                == Some(replay::UnsentDecision::Fold) =>
        {
            "the turn was abandoned after its request was refused as over the context window; compaction did not start"
        }
        AbandonedTurnClaim::ToolRoundCompleted => {
            if model_round_boundary(source, fold.turn_id, fold.root_operation) {
                "the turn was interrupted after its model round completed, before the next model request"
            } else {
                "the turn was interrupted after its tool round completed, before the next model request"
            }
        }
        _ if root_is_only_turn_operation(
            source.manifest().ledger(),
            fold.turn_id,
            fold.root_operation,
        ) =>
        {
            "the turn was abandoned after the process was lost, before any work started"
        }
        _ => {
            "the turn was abandoned after the process was lost; the tool its last round asked for never started"
        }
    }
}
