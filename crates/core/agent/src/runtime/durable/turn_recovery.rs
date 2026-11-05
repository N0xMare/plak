//! Exact-open closure and local terminal recovery.

#[cfg(test)]
#[path = "turn_recovery_review_tests.rs"]
pub(in crate::runtime::durable) mod review_tests;

#[cfg(test)]
mod landed_control_guard_tests;

#[cfg(test)]
mod closure_parity_tests;

mod abandonment;
mod abandonment_rows;
#[cfg(test)]
mod admission_compaction;
pub(in crate::runtime) mod controls;
mod fold_rows;
mod in_turn_compaction;
mod publication;
mod replay;
mod terminal;
mod terminal_siblings;
mod verdict;

pub(in crate::runtime) use publication::recover_root_turn_publication;

use terminal::{local_terminal_from_ledger, terminal_evidence_fold, terminal_turn_usage};

use abandonment::{
    abandoned_compaction_evidence, abandoned_effect_evidence, abandoned_operation_evidence,
    abandoned_process_evidence, abandoned_turn_segment, abandoned_turn_segment_with_note,
    abandonment_evidence_fold, close_as_unobserved, finalize_not_started,
    require_readable_provider_records, root_is_only_turn_operation, root_turn_command,
};
pub(in crate::runtime) use abandonment::{abandoned_turn_claim, abandonment_cause};

use abandonment_rows::{ClosureFacts, TurnFoldFacts, TurnRow};
pub use verdict::{CheckpointRecoveryVerdict, checkpoint_recovery_verdict};
pub(in crate::runtime) use verdict::{RecoveryEvidence, RecoveryVerdict, recovery_verdict};

use crate::runtime::checkpoint::identity::history_segment_kind;
use crate::runtime::checkpoint::io::DurabilityCommitTelemetry;
use crate::runtime::continuation::{
    runtime_component, runtime_manifest_error, terminal_record_for_command,
};
use crate::runtime::durable::bootstrap::checkpoint_has_code_cell_obligation;
use crate::runtime::durable::composition_recovery::{
    code_cell_abandonment_fold, code_cell_abandonment_state, pending_steering, queued_root,
    reopen_code_cell_abandonment_identity,
};
use crate::runtime::durable::{
    AbandonedRoot, AbandonedTurnRecoveryFold, DurableCodeCellAbandonmentDisposition,
    DurableSessionAddress, LocalTerminalRecoveryFold,
};
use crate::runtime::operation_record::{
    RoundDecision, is_accepted_root, operation_turn_id, provider_outcome, working_lifecycle,
};
use crate::runtime::terminal::{
    DurableAgentError, DurableTerminalOutcome, PendingTerminalResultRecord,
    TERMINAL_RESULT_RECORD_VERSION, decode_pending_terminal_record, encode_pending_terminal_record,
    finalized_operation_evidence,
};
use crate::runtime::{AbandonedTurnClaim, AbandonmentCause, OperationId};
use crate::{
    AgentError, AgentErrorKind, BranchHead, BranchLease, BranchRevision, CheckpointId,
    CheckpointObjectBundle, CommandId, DurableCommandDisposition, DurableCommandRecord,
    DurableOperationKind, DurableOperationRecord, DurableOperationState,
    DurableOperationTransition, ReconstructiveComponent, RecoveryAction, RecoveryObligation,
    RecoveryObligationKind, RuntimeCheckpoint, RuntimeComponentKind, RuntimeDurability,
    RuntimeLifecycle, RuntimeOperationLedger, RuntimeWorkingRoot, SessionStore, SessionStoreError,
    StoredCheckpoint, TurnUsage, WorkingRecoveryPhase,
};
use plakous_model::{
    Actor, ContentBlock, ConversationItem, Digest, DigestDomain, EntryId, HistorySegment,
    HistorySegmentKind, ModelOperationRoot, ModelResponse, PreparedCallRecord, Provenance,
    TextBlock, TurnId, WorkingProjection,
};
use std::sync::Arc;
use std::time::Duration;

pub(super) fn validate_loaded_address(
    loaded: &crate::LoadedBranch,
    expected: DurableSessionAddress,
    durability: RuntimeDurability,
) -> Result<(), AgentError> {
    let session = loaded.session();
    let head = loaded.head();
    let stored = loaded.checkpoint();
    let reference = stored.reference();
    let checkpoint = stored.checkpoint();
    let topology = checkpoint.manifest().topology();
    if session.is_archived()
        || session.session_id() != expected.session_id
        || session.durability() != durability
        || head.session() != expected.session()
        || reference != head.checkpoint()
        || reference.checkpoint_id() != expected.checkpoint_id
        || topology.session_id != expected.session_id
        || topology.branch_id != expected.branch_id
        || topology.checkpoint_id != expected.checkpoint_id
        || topology.branch_revision != head.revision().get()
        || checkpoint.integrity() != reference.integrity()
        || checkpoint.manifest().durability() != durability
    {
        return Err(AgentError::new(
            AgentErrorKind::Session,
            "durable session address does not name the exact current checkpoint",
        ));
    }
    Ok(())
}

pub(super) fn validate_historical_address(
    stored: &StoredCheckpoint,
    expected: DurableSessionAddress,
    durability: RuntimeDurability,
) -> Result<BranchHead, AgentError> {
    let reference = stored.reference();
    let checkpoint = stored.checkpoint();
    let topology = checkpoint.manifest().topology();
    if reference.checkpoint_id() != expected.checkpoint_id
        || topology.session_id != expected.session_id
        || topology.branch_id != expected.branch_id
        || topology.checkpoint_id != expected.checkpoint_id
        || checkpoint.integrity() != reference.integrity()
        || checkpoint.manifest().durability() != durability
    {
        return Err(AgentError::new(
            AgentErrorKind::Session,
            "durable session address does not name its exact historical checkpoint",
        ));
    }
    Ok(BranchHead::from_store(
        expected.session(),
        BranchRevision::from_store(topology.branch_revision),
        reference,
    ))
}

pub(in crate::runtime) fn checkpoint_requires_closed_recovery(
    checkpoint: &RuntimeCheckpoint,
) -> bool {
    matches!(
        checkpoint.manifest().lifecycle(),
        RuntimeLifecycle::Working { .. }
    )
}

/// What an open does with a head whose turn waits at an effect decision.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum DecisionBarrier {
    /// Restore the turn at its barrier, to be decided.
    #[default]
    Restore,
    /// Close the turn as abandoned with the cause the caller asserts: the
    /// decision was never made, so the effect it guarded never started.
    Close(AbandonmentCause),
}

pub(in crate::runtime) enum RootClosure {
    FinalizeRecorded {
        terminal: LocalTerminalRecoveryFold,
        code_cell: Option<Box<super::CodeCellAbandonmentFold>>,
    },
    Complete(LocalTerminalRecoveryFold),
    Abandon {
        fold: AbandonedTurnRecoveryFold,
        claim: AbandonedTurnClaim,
    },
}

impl RootClosure {
    pub(in crate::runtime) fn command_id(&self) -> &CommandId {
        match self {
            Self::FinalizeRecorded { terminal, .. } | Self::Complete(terminal) => {
                &terminal.command_id
            }
            Self::Abandon { fold, .. } => &fold.command_id,
        }
    }

    #[cfg(test)]
    pub(in crate::runtime) fn terminal(&self) -> Option<&LocalTerminalRecoveryFold> {
        match self {
            Self::FinalizeRecorded { terminal, .. } | Self::Complete(terminal) => Some(terminal),
            Self::Abandon { .. } => None,
        }
    }
}

/// Pure decision over the exact durable cut. The guards retain the current
/// closure scope; the classifier is not authority to widen that scope.
fn newest_root_turn_closure(
    checkpoint: &RuntimeCheckpoint,
    barrier: DecisionBarrier,
) -> Result<Option<RootClosure>, AgentError> {
    if !checkpoint_requires_closed_recovery(checkpoint) {
        return Ok(None);
    }
    let cell = if checkpoint_has_code_cell_obligation(checkpoint) {
        match code_cell_abandonment_fold(checkpoint) {
            Ok(cell) => Some(cell),
            Err(error) if error.kind() == AgentErrorKind::OutcomeUnknown => None,
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    if let Some(cell) = cell
        && !cell.operations.is_empty()
        && !cell.turn_abandonment
    {
        let identity = reopen_code_cell_abandonment_identity(checkpoint)?;
        let (_, ledger, lifecycle) =
            code_cell_abandonment_state(checkpoint, identity, checkpoint.id())?;
        let terminal = local_terminal_from_ledger(checkpoint, &ledger, &lifecycle, false)?;
        return Ok(Some(RootClosure::FinalizeRecorded {
            terminal,
            code_cell: Some(Box::new(cell)),
        }));
    }
    let refusal = match terminal_evidence_fold(checkpoint) {
        Ok(terminal) => {
            return Ok(Some(if terminal.synthesized.is_some() {
                RootClosure::Complete(terminal)
            } else {
                RootClosure::FinalizeRecorded {
                    terminal,
                    code_cell: None,
                }
            }));
        }
        Err(error) if error.kind() == AgentErrorKind::OutcomeUnknown => error,
        Err(error) => return Err(error),
    };
    match abandonment_evidence_fold(checkpoint, barrier) {
        Ok(fold) => Ok(Some(RootClosure::Abandon {
            claim: abandoned_turn_claim(&fold),
            fold,
        })),
        Err(error) if error.kind() == AgentErrorKind::OutcomeUnknown => {
            // Retain every existing closure (including its abandonment report)
            // before widening a previously refused recorded-terminal cut.
            match local_terminal_from_ledger(
                checkpoint,
                checkpoint.manifest().ledger(),
                checkpoint.manifest().lifecycle(),
                true,
            ) {
                Ok(terminal) => Ok(Some(RootClosure::FinalizeRecorded {
                    terminal,
                    code_cell: None,
                })),
                Err(error) if error.kind() == AgentErrorKind::OutcomeUnknown => Err(refusal),
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
pub(in crate::runtime) fn local_terminal_recovery_fold(
    checkpoint: &RuntimeCheckpoint,
) -> Result<LocalTerminalRecoveryFold, AgentError> {
    match newest_root_turn_closure(checkpoint, DecisionBarrier::Restore)? {
        Some(RootClosure::FinalizeRecorded { terminal, .. } | RootClosure::Complete(terminal)) => {
            Ok(terminal)
        }
        _ => Err(AgentError::new(
            AgentErrorKind::OutcomeUnknown,
            "not a terminal closure",
        )),
    }
}

#[cfg(test)]
pub(in crate::runtime) fn abandoned_turn_recovery_fold(
    checkpoint: &RuntimeCheckpoint,
) -> Result<AbandonedTurnRecoveryFold, AgentError> {
    match newest_root_turn_closure(checkpoint, DecisionBarrier::Restore) {
        Ok(Some(RootClosure::Abandon { fold, .. })) => Ok(fold),
        _ => abandonment_evidence_fold(checkpoint, DecisionBarrier::Restore),
    }
}

/// The closure a test cut's open would publish, read through the predicate.
#[cfg(test)]
pub(in crate::runtime) fn root_turn_closure_for_test(
    checkpoint: &RuntimeCheckpoint,
) -> Result<Option<RootClosure>, AgentError> {
    newest_root_turn_closure(checkpoint, DecisionBarrier::Restore)
}

#[cfg(test)]
pub(in crate::runtime::durable) fn close_root_turn_for_test(
    checkpoint: &RuntimeCheckpoint,
) -> Result<(plakous_model::CanonicalHistory, RuntimeOperationLedger), AgentError> {
    let Some(RootClosure::Abandon { fold, claim }) =
        newest_root_turn_closure(checkpoint, DecisionBarrier::Restore)?
    else {
        return Err(AgentError::new(
            AgentErrorKind::Session,
            "test cut is not an abandoned root",
        ));
    };
    let target = CheckpointId::try_from(uuid::Uuid::from_u128(0xA2_FFFF))
        .map_err(|error| AgentError::from_identity(&error))?;
    let closed = abandonment_closure_state(checkpoint, fold, claim, target)?;
    Ok((closed.history, closed.ledger))
}

/// Whether a newly opened owner closes this working head itself. Such a head
/// is not recovery-only; opening it is how it recovers.
pub(crate) fn reopen_closes_working_turn(checkpoint: &RuntimeCheckpoint) -> bool {
    reopen_closes_working_turn_at(checkpoint, DecisionBarrier::Restore)
}

/// [`reopen_closes_working_turn`] for an open that treats an effect-decision
/// barrier as `barrier` says.
pub(crate) fn reopen_closes_working_turn_at(
    checkpoint: &RuntimeCheckpoint,
    barrier: DecisionBarrier,
) -> bool {
    recovery_verdict(RecoveryEvidence::head(checkpoint).with_barrier(barrier)).closes()
}

/// The closure the one predicate assigns to this head: `None` for a head it
/// does not close, an error for a refusal.
pub(in crate::runtime) fn verdict_closure(
    checkpoint: &RuntimeCheckpoint,
) -> Result<Option<RootClosure>, AgentError> {
    verdict_closure_at(checkpoint, DecisionBarrier::Restore)
}

/// [`verdict_closure`] for an open that treats an effect-decision barrier as
/// `barrier` says.
pub(in crate::runtime) fn verdict_closure_at(
    checkpoint: &RuntimeCheckpoint,
    barrier: DecisionBarrier,
) -> Result<Option<RootClosure>, AgentError> {
    recovery_verdict(RecoveryEvidence::head(checkpoint).with_barrier(barrier)).into_closure()
}

/// The session-workflow row the one predicate assigns to this head, if any.
pub(in crate::runtime) fn verdict_workflow_closure(
    checkpoint: &RuntimeCheckpoint,
) -> Option<crate::runtime::durable::workflow_closure::WorkflowClosure> {
    match recovery_verdict(RecoveryEvidence::head(checkpoint)) {
        RecoveryVerdict::SettleWorkflow(closure) => Some(*closure),
        _ => None,
    }
}

/// The typed integrity failure the one predicate refuses this head with.
pub(in crate::runtime) fn verdict_refusal(checkpoint: &RuntimeCheckpoint) -> Option<AgentError> {
    match recovery_verdict(RecoveryEvidence::head(checkpoint)) {
        RecoveryVerdict::Refuse(refusal) => Some(refusal.into_error()),
        _ => None,
    }
}

/// Whether the one predicate refuses this head as typed integrity.
pub(crate) fn recovery_verdict_refuses(checkpoint: &RuntimeCheckpoint) -> bool {
    recovery_verdict(RecoveryEvidence::head(checkpoint)).refuses()
}

/// Whether a newly opened owner abandons this head's unfinished turn, which
/// carries no definite result a record could have missed.
#[cfg(test)]
pub(in crate::runtime) fn reopen_abandons_working_turn(checkpoint: &RuntimeCheckpoint) -> bool {
    match recovery_verdict(RecoveryEvidence::head(checkpoint)) {
        RecoveryVerdict::Close(closure) => matches!(
            *closure,
            RootClosure::Abandon { .. }
                | RootClosure::FinalizeRecorded {
                    code_cell: Some(_),
                    ..
                }
        ),
        _ => false,
    }
}

/// Whether the turn's dispatched root rests on a promoted assistant
/// continuation (`Continue`), for the model-round wording.
pub(in crate::runtime) fn model_round_boundary(
    checkpoint: &RuntimeCheckpoint,
    fold: &AbandonedTurnRecoveryFold,
) -> bool {
    replay::closing_round_boundary(
        checkpoint.manifest().ledger(),
        fold.turn_id,
        fold.root_operation,
        fold.closed_continuation,
    )
    .is_ok_and(|boundary| boundary == Some(replay::RoundBoundary::Model))
}

/// The decided retry or overflow fold the turn's dispatched root rests on,
/// for the note that says it was not sent.
fn unsent_decision(
    checkpoint: &RuntimeCheckpoint,
    turn_id: TurnId,
    root: OperationId,
) -> Option<replay::UnsentDecision> {
    replay::unsent_decision(checkpoint.manifest().ledger(), turn_id, root)
        .ok()
        .flatten()
}

/// A process session whose outcome no record establishes: recovery closes it
/// as outcome unknown and never signals its orphan.
fn last_root_turn(ledger: &RuntimeOperationLedger) -> Result<TurnId, AgentError> {
    ledger
        .commands()
        .into_iter()
        .filter(|command| is_accepted_root(command))
        .filter_map(DurableCommandRecord::turn_id)
        .next_back()
        .ok_or_else(|| {
            AgentError::new(
                AgentErrorKind::OutcomeUnknown,
                "an idle workflow has no turn to abandon",
            )
        })
}

fn abandoned_turn_entry_id(turn_id: TurnId) -> Result<EntryId, AgentError> {
    let digest = Digest::sha256(
        DigestDomain::ProviderOperation,
        2,
        &[
            b"plakous-agent/abandoned-turn-entry-v1",
            turn_id.as_uuid().as_bytes(),
        ],
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Identity,
            "abandoned turn entry identity failed",
        )
    })?;
    let bytes = crate::derived_id::bytes(digest);
    EntryId::try_from(uuid::Uuid::from_bytes(bytes)).map_err(|_| {
        AgentError::new(
            AgentErrorKind::Identity,
            "abandoned turn entry identity is invalid",
        )
    })
}

const fn uncertain_process_state(state: &DurableOperationState) -> bool {
    matches!(
        state,
        DurableOperationState::Prepared
            | DurableOperationState::Claimed(_)
            | DurableOperationState::Dispatched
            | DurableOperationState::LocatorBound(_)
            | DurableOperationState::OutcomeUnknown(_)
    )
}

/// The note for one claim under one cause. Each asserts only its own claim:
/// no billing warning where nothing reached a provider, and no workspace
/// warning where no command could have run.
fn abandonment_closure_state(
    source: &RuntimeCheckpoint,
    fold: AbandonedTurnRecoveryFold,
    claim: AbandonedTurnClaim,
    checkpoint_id: CheckpointId,
) -> Result<RootClosureState, AgentError> {
    let source_manifest = source.manifest();
    let mut ledger = source_manifest.ledger().clone();
    for operation in &fold.settled_operations {
        let record = ledger
            .operation(operation.0)
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
        if controls::same_turn_control(&ledger, record, fold.turn_id) {
            ledger = controls::settle_control(&ledger, record, checkpoint_id)?;
        }
    }
    // A fold row words its own note when its claim is the strongest one.
    let row = if code_cell_abandonment_fold(source).is_ok_and(|cell| cell.turn_abandonment) {
        None
    } else {
        fold_rows::fold_row(source, fold.turn_id, fold.root_operation)?
            .filter(|row| row.claim() == claim)
    };
    let facts = ClosureFacts {
        source,
        fold: &fold,
        claim,
        cause: abandonment_cause(&fold),
        row,
        completed_effect_only: !fold.uncommitted_effect_operations.is_empty()
            && fold.effect_operations.is_empty()
            && fold.process_operations.is_empty(),
    };
    let segment = abandonment_rows::abandonment_segment(&facts)?;
    let segment = if controls::has_unapplied_steering(&ledger, fold.turn_id) {
        let [ContentBlock::Text(text)] = segment.items()[0].blocks() else {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        };
        abandoned_turn_segment_with_note(
            fold.turn_id,
            &format!(
                "{} Queued steering was not applied and is not retried.",
                text.text()
            ),
        )?
    } else {
        segment
    };
    let history = source_manifest
        .canonical()
        .history()
        .clone()
        .append(segment)
        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
    let ledger = abandonment_rows::close_uncertain_operations(ledger, &fold, checkpoint_id)?;
    let ledger = abandonment_rows::finalize_retained_operations(
        ledger,
        source,
        &fold,
        checkpoint_id,
        history.identity(),
    )?;
    let ledger = abandonment_rows::close_root(ledger, &facts, checkpoint_id, history.identity())?;
    Ok(RootClosureState {
        history,
        ledger,
        turn_id: fold.turn_id,
        command_id: fold.command_id,
        family_artifact: None,
    })
}

fn terminal_closure_state(
    source: &RuntimeCheckpoint,
    fold: LocalTerminalRecoveryFold,
    code_cell: bool,
    checkpoint_id: CheckpointId,
) -> Result<RootClosureState, AgentError> {
    let source_manifest = source.manifest();
    let (mut history, mut ledger) = if code_cell {
        let identity = reopen_code_cell_abandonment_identity(source)?;
        let (history, ledger, _) = code_cell_abandonment_state(source, identity, checkpoint_id)?;
        (history, ledger)
    } else {
        (
            source_manifest.canonical().history().clone(),
            source_manifest.ledger().clone(),
        )
    };
    if fold.withhold_prompt {
        let mut retained = history.segments().to_vec();
        retained.pop();
        history = plakous_model::CanonicalHistory::new(retained)
            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
    }
    if !fold.output.is_empty() {
        history = history
            .append(
                HistorySegment::new(history_segment_kind(&fold.output), fold.output)
                    .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?,
            )
            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
    }
    if let Some(tail) = fold.tail {
        history = history
            .append(tail)
            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
    }
    for operation in &fold.operations {
        if let Some(record) = ledger.operation(operation.0)
            && controls::same_turn_control(&ledger, record, fold.turn_id)
        {
            ledger = controls::settle_control(&ledger, record, checkpoint_id)?;
            continue;
        }
        if let Some(record) = ledger.operation(operation.0)
            && matches!(record.kind(), DurableOperationKind::ProcessSession { .. })
            && uncertain_process_state(record.state())
        {
            let evidence = match record.state() {
                DurableOperationState::OutcomeUnknown(evidence) => evidence.clone(),
                _ => abandoned_process_evidence()?,
            };
            ledger = close_as_unobserved(&ledger, *operation, &evidence, checkpoint_id)?;
            continue;
        }
        if let Some(settled) =
            terminal_siblings::settle_uncertain(source, &ledger, *operation, checkpoint_id)?
        {
            ledger = settled;
            continue;
        }
        if let Some((root, evidence)) = &fold.synthesized
            && root == operation
        {
            ledger = ledger
                .transition_operation(
                    operation.0,
                    DurableOperationTransition::new(
                        checkpoint_id,
                        DurableOperationState::Completed(evidence.clone()),
                    ),
                )
                .map_err(runtime_manifest_error)?;
        }
        let evidence = ledger
            .operation(operation.0)
            .and_then(|record| match record.state() {
                DurableOperationState::Completed(evidence)
                | DurableOperationState::ProvenNotDispatched(evidence) => Some(evidence),
                _ => None,
            })
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
        let finalized = finalized_operation_evidence(evidence, checkpoint_id, history.identity())?;
        ledger = ledger
            .transition_operation(
                operation.0,
                DurableOperationTransition::new(
                    checkpoint_id,
                    DurableOperationState::Finalized(finalized),
                ),
            )
            .map_err(runtime_manifest_error)?;
    }
    Ok(RootClosureState {
        history,
        ledger,
        turn_id: fold.turn_id,
        command_id: fold.command_id,
        family_artifact: fold.family_artifact,
    })
}

struct RootClosureState {
    history: plakous_model::CanonicalHistory,
    ledger: RuntimeOperationLedger,
    turn_id: TurnId,
    command_id: CommandId,
    family_artifact: Option<crate::FamilyArtifact>,
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit test assertions, matching the sibling durable tests"
)]
mod abandoned_turn_note_tests;

#[cfg(test)]
mod abandoned_turn_position_tests;

#[cfg(test)]
mod closure_tag_tests;
