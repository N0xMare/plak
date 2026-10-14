//! Exact-open closure and local terminal recovery.

#[cfg(test)]
#[path = "turn_recovery_review_tests.rs"]
pub(in crate::runtime::durable) mod review_tests;

#[cfg(test)]
mod landed_control_guard_tests;

#[cfg(test)]
mod closure_parity_tests;

#[cfg(test)]
mod admission_compaction;
pub(in crate::runtime) mod controls;
mod fold_rows;
mod in_turn_compaction;
mod replay;
mod terminal_siblings;
mod verdict;

pub use verdict::{CheckpointRecoveryVerdict, checkpoint_recovery_verdict};
pub(in crate::runtime) use verdict::{RecoveryEvidence, RecoveryVerdict, recovery_verdict};

#[cfg(test)]
use crate::recovery_scope::require_recoverable;
use crate::runtime::checkpoint::identity::{
    CheckpointCommitFacts, checkpoint_commit_identity, history_segment_kind,
};
use crate::runtime::checkpoint::io::{
    DurabilityCommitTelemetry, commit_with_acknowledgement_recovery,
};
use crate::runtime::continuation::{
    checkpoint_bundle_error, runtime_component, runtime_manifest_error,
    terminal_record_at_checkpoint, terminal_record_for_command,
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
    CheckpointObjectBundle, CheckpointObjectRef, CommandId, CommitCheckpoint,
    DurableCommandDisposition, DurableCommandRecord, DurableOperationKind, DurableOperationRecord,
    DurableOperationState, DurableOperationTransition, ReconstructiveComponent, RecoveryAction,
    RecoveryObligation, RecoveryObligationKind, RuntimeCanonicalState, RuntimeCheckpoint,
    RuntimeCheckpointCodec, RuntimeComponentKind, RuntimeDurability, RuntimeLifecycle,
    RuntimeManifest, RuntimeOperationLedger, RuntimeTopology, RuntimeWorkingRoot, SessionStore,
    SessionStoreError, StoreAdmission, StoredCheckpoint, TurnUsage, WorkingRecoveryPhase,
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
    match abandonment_evidence_fold(checkpoint) {
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
    match newest_root_turn_closure(checkpoint)? {
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
    match newest_root_turn_closure(checkpoint) {
        Ok(Some(RootClosure::Abandon { fold, .. })) => Ok(fold),
        _ => abandonment_evidence_fold(checkpoint),
    }
}

/// The closure a test cut's open would publish, read through the predicate.
#[cfg(test)]
pub(in crate::runtime) fn root_turn_closure_for_test(
    checkpoint: &RuntimeCheckpoint,
) -> Result<Option<RootClosure>, AgentError> {
    newest_root_turn_closure(checkpoint)
}

/// Whether a newly opened owner closes this working head itself. Such a head
/// is not recovery-only; opening it is how it recovers.
pub(crate) fn reopen_closes_working_turn(checkpoint: &RuntimeCheckpoint) -> bool {
    recovery_verdict(RecoveryEvidence::head(checkpoint)).closes()
}

/// The closure the one predicate assigns to this head: `None` for a head it
/// does not close, an error for a refusal.
pub(in crate::runtime) fn verdict_closure(
    checkpoint: &RuntimeCheckpoint,
) -> Result<Option<RootClosure>, AgentError> {
    recovery_verdict(RecoveryEvidence::head(checkpoint)).into_closure()
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
    turn_id: TurnId,
    root: OperationId,
) -> bool {
    replay::finalized_round_boundary(checkpoint.manifest().ledger(), turn_id, root)
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

/// Reads the one plan that abandons a working root turn.
///
/// Two things leave a turn unfinished. A process that dies mid-turn — a signal
/// past the graceful deadline, a `SIGKILL`, a crash — leaves the turn's command
/// dispatched with no result. A live owner that loses sight of one outcome
/// delivers `RecoveryRequired` and finalizes the command, but leaves the
/// operation it could not see outstanding. Nothing can establish those outcomes
/// afterwards: recovery classification is read-only, and
/// `LanguageModel::recover_call` is unsupported on every route. So the only way
/// forward is to record the uncertainty and continue, and this fold is the one
/// authority for what may be closed that way. Only a newly opened owner reads
/// it, before that owner accepts work.
///
/// It accepts provider attempts, tool effects, joined process sessions and one
/// compaction. It refuses everything else: a pending effect decision or hosted
/// continuation is restored instead of abandoned, and a commit obligation is
/// settled by commit recovery before this fold ever sees the head.
///
/// # Errors
///
/// Returns `OutcomeUnknown` when the checkpoint is anything but such a turn,
/// and a corrupt-store error when one of the turn's provider records does not
/// decode.
fn abandonment_evidence_fold(
    checkpoint: &RuntimeCheckpoint,
) -> Result<AbandonedTurnRecoveryFold, AgentError> {
    let refuse = |message: &'static str| AgentError::new(AgentErrorKind::OutcomeUnknown, message);
    let manifest = checkpoint.manifest();
    let RuntimeLifecycle::Working {
        root, obligations, ..
    } = manifest.lifecycle()
    else {
        return Err(refuse("abandoning a turn requires one recovery-only root"));
    };
    let ledger = manifest.ledger();
    let turn_id = match root {
        RuntimeWorkingRoot::Turn { turn_id, .. } => *turn_id,
        // A compaction fenced after its turn already delivered
        // recovery-required leaves only the compaction outstanding, so the
        // working root names the compaction's workflow. The turn it interrupted
        // is the last admitted root turn, and the root check below requires it
        // to be terminal as recovery-required.
        RuntimeWorkingRoot::SessionWorkflow(_) => last_root_turn(ledger)?,
    };
    require_readable_provider_records(ledger, turn_id)?;
    let root_command = root_turn_command(ledger, turn_id)?;
    let root_dispatched = ledger.operations().into_iter().any(|record| {
        matches!(record.kind(), DurableOperationKind::Command { turn_id: Some(bound) }
            if *bound == turn_id)
            && record.request().bytes() == root_command.command_component().bytes()
            && matches!(record.state(), DurableOperationState::Dispatched)
    });
    // P is Completed with a ToolBatch decision: every batch record that
    // completed keeps its own evidence and its result was never added.
    let batch_pending = root_dispatched && replay::completed_tool_batch(ledger, turn_id)?;
    let mut provider_operations = Vec::with_capacity(obligations.len());
    let mut settled_operations = Vec::with_capacity(obligations.len());
    let mut uncommitted_effect_operations = Vec::new();
    let mut effect_operations = Vec::with_capacity(obligations.len());
    let mut unrun_effect_operations = Vec::with_capacity(obligations.len());
    let mut process_operations = Vec::new();
    let mut compaction_operations = Vec::new();
    let mut compaction_workflow = None;
    let mut compaction_records = 0_usize;
    let mut root_operation = None;
    // A terminal root needs only a turn-level uncertainty note, not another
    // outer Code Cell call/result exchange.
    let code_cell = code_cell_abandonment_fold(checkpoint)
        .ok()
        .filter(|fold| fold.turn_abandonment && !fold.operations.is_empty());
    for obligation in obligations {
        let record = ledger
            .operation(obligation.operation_id())
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
        if controls::same_turn_control(ledger, record, turn_id)
            && obligation.kind() == RecoveryObligationKind::Command
            && matches!(
                (obligation.action(), record.state()),
                (
                    RecoveryAction::ProvenNotDispatched,
                    DurableOperationState::Prepared
                ) | (
                    RecoveryAction::FinalizationRequired,
                    DurableOperationState::Dispatched | DurableOperationState::Completed(_)
                )
            )
        {
            settled_operations.push(OperationId(record.operation_id()));
            continue;
        }
        if let Some(fold) = &code_cell {
            if pending_steering(checkpoint, record, turn_id) {
                settled_operations.push(OperationId(record.operation_id()));
                continue;
            }
            if queued_root(checkpoint, record, turn_id) {
                unrun_effect_operations.push(OperationId(record.operation_id()));
                continue;
            }
            if let Some((operation, disposition)) = fold
                .operations
                .iter()
                .find(|(operation, _)| operation.0 == record.operation_id())
            {
                match disposition {
                    DurableCodeCellAbandonmentDisposition::PossiblyStarted => {
                        effect_operations.push(*operation);
                    }
                    DurableCodeCellAbandonmentDisposition::ProvenNotDispatched => {
                        unrun_effect_operations.push(*operation);
                    }
                }
                continue;
            }
        }
        // A compaction and its provider attempt belong to the compaction's
        // workflow, not to the turn, and neither was applied: a compaction's
        // effect exists only through its own commit, which never happened.
        let workflow = match record.kind() {
            DurableOperationKind::Compaction { workflow_id }
            | DurableOperationKind::Provider {
                root: ModelOperationRoot::SessionWorkflow(workflow_id),
                ..
            } => Some(*workflow_id),
            _ => None,
        };
        if let Some(workflow) = workflow {
            if !in_turn_compaction::abandonable_debt(record, obligation)
                || compaction_workflow
                    .replace(workflow)
                    .is_some_and(|seen| seen != workflow)
            {
                return Err(refuse(
                    "this checkpoint's compaction is outside abandonment scope",
                ));
            }
            if matches!(record.kind(), DurableOperationKind::Compaction { .. }) {
                compaction_records = compaction_records.saturating_add(1);
            }
            compaction_operations.push(OperationId(record.operation_id()));
            continue;
        }
        if operation_turn_id(record) != Some(turn_id) {
            return Err(refuse(
                "abandoning a turn requires every obligation to be its own",
            ));
        }
        match (obligation.kind(), obligation.action(), record.state()) {
            (
                RecoveryObligationKind::Command,
                RecoveryAction::ProvenNotDispatched,
                DurableOperationState::Prepared,
            ) if root_operation.is_none()
                && (code_cell.is_some()
                    || root_is_only_turn_operation(
                        ledger,
                        turn_id,
                        OperationId(record.operation_id()),
                    )) =>
            {
                root_operation = Some(OperationId(record.operation_id()));
            }
            (
                RecoveryObligationKind::Command,
                RecoveryAction::FinalizationRequired,
                DurableOperationState::Completed(_),
            ) if code_cell.is_some() => {}
            (
                RecoveryObligationKind::CodeCell | RecoveryObligationKind::Effect,
                RecoveryAction::FinalizationRequired,
                DurableOperationState::Completed(_) | DurableOperationState::ProvenNotDispatched(_),
            ) if code_cell.is_some() => settled_operations.push(OperationId(record.operation_id())),
            (
                RecoveryObligationKind::Effect,
                RecoveryAction::FinalizationRequired,
                DurableOperationState::Completed(_),
            ) if root_dispatched
                && ledger.operations().into_iter().any(|provider| {
                    matches!((record.kind(), provider.kind()),
                    (DurableOperationKind::Effect { attempt_id: effect_attempt, .. },
                     DurableOperationKind::Provider { attempt_id: provider_attempt, .. })
                    if effect_attempt == provider_attempt)
                        && operation_turn_id(provider) == Some(turn_id)
                        && matches!(provider.state(), DurableOperationState::Completed(_))
                }) =>
            {
                let operation = OperationId(record.operation_id());
                settled_operations.push(operation);
                uncommitted_effect_operations.push(operation);
            }
            (
                RecoveryObligationKind::CodeCell,
                RecoveryAction::FinalizationRequired,
                DurableOperationState::Completed(_),
            ) if batch_pending => {
                let operation = OperationId(record.operation_id());
                settled_operations.push(operation);
                uncommitted_effect_operations.push(operation);
            }
            (
                RecoveryObligationKind::Command,
                RecoveryAction::FinalizationRequired,
                DurableOperationState::Dispatched,
            ) if root_operation.is_none() => {
                root_operation = Some(OperationId(record.operation_id()));
            }
            (
                RecoveryObligationKind::ProviderAttempt,
                RecoveryAction::ProviderEvidenceRequired,
                DurableOperationState::Prepared
                | DurableOperationState::Dispatched
                | DurableOperationState::LocatorBound(_)
                | DurableOperationState::OutcomeUnknown(_),
            ) => provider_operations.push(OperationId(record.operation_id())),
            // Completed rounds and joined processes retain their exact evidence.
            // Another obligation may still be unfinished, but these are not
            // uncertain and cannot justify abandonment on their own.
            (
                RecoveryObligationKind::ProviderAttempt | RecoveryObligationKind::ProcessSession,
                RecoveryAction::FinalizationRequired,
                DurableOperationState::Completed(_),
            ) => settled_operations.push(OperationId(record.operation_id())),
            (
                RecoveryObligationKind::Effect,
                RecoveryAction::EffectEvidenceRequired,
                DurableOperationState::Prepared
                | DurableOperationState::Claimed(_)
                | DurableOperationState::Dispatched
                | DurableOperationState::OutcomeUnknown(_),
            ) => effect_operations.push(OperationId(record.operation_id())),
            // The one effect state that is evidence rather than silence.
            // Closing it as uncertain would put a warning about the workspace
            // in front of a user over a command this session can prove never
            // started.
            (
                RecoveryObligationKind::Effect,
                RecoveryAction::EffectEvidenceRequired,
                DurableOperationState::ProvenNotDispatched(_),
            ) => unrun_effect_operations.push(OperationId(record.operation_id())),
            (
                RecoveryObligationKind::ProcessSession,
                RecoveryAction::EffectEvidenceRequired,
                DurableOperationState::Prepared | DurableOperationState::Dispatched,
            ) => process_operations.push(OperationId(record.operation_id())),
            _ => {
                return Err(refuse(
                    "this checkpoint is outstanding on more than work a turn can abandon",
                ));
            }
        }
    }
    if !compaction_operations.is_empty() && compaction_records != 1 {
        return Err(refuse("an abandoned compaction must be exactly one"));
    }
    // A root dispatched with no result whose last round landed asking the
    // harness for a tool is unfinished too: the process was lost before it
    // prepared that tool. Every tool, cell and nested call is durably prepared
    // before it dispatches (a Code Mode `exec` is shown as started first), so
    // the absence of one proves that tool never started. Completed tool or
    // process evidence owing finalization is not that shape: something ran. A
    // landed round that did not ask the harness for a tool — text only, or a
    // provider-hosted call the provider already ran — keeps its own recovery
    // rule.
    let asked_for_a_tool = replay::completed_tool_batch(ledger, turn_id)?;
    let root_without_work = code_cell.is_none()
        && root_operation.is_some_and(|operation| {
            root_is_only_turn_operation(ledger, turn_id, operation)
                && ledger.operations().into_iter().any(|record| {
                    record.operation_id() == operation.0
                        && matches!(
                            record.state(),
                            DurableOperationState::Prepared | DurableOperationState::Dispatched
                        )
                        && root_turn_command(ledger, turn_id).is_ok_and(|command| {
                            record.request().bytes() == command.command_component().bytes()
                        })
                })
        });
    let tool_round_completed = match (code_cell.is_none(), root_operation) {
        (true, Some(operation)) => {
            replay::finalized_round_boundary(ledger, turn_id, operation)?.is_some()
        }
        _ => false,
    };
    let decision_unsent = match (code_cell.is_none(), root_operation) {
        (true, Some(operation)) => replay::unsent_decision(ledger, turn_id, operation)?.is_some(),
        _ => false,
    };
    let fold_row = match (code_cell.is_none(), root_operation) {
        (true, Some(operation)) => fold_rows::fold_row(checkpoint, turn_id, operation)?,
        _ => None,
    };
    let root_dispatched_without_result = code_cell.is_none()
        && asked_for_a_tool
        && settled_operations.iter().all(|operation| {
            ledger.operations().into_iter().any(|record| {
                record.operation_id() == operation.0
                    && (matches!(record.kind(), DurableOperationKind::Provider { .. })
                        || controls::same_turn_control(ledger, record, turn_id))
            })
        })
        && root_operation.is_some_and(|operation| {
            ledger.operations().into_iter().any(|record| {
                record.operation_id() == operation.0
                    && matches!(record.state(), DurableOperationState::Dispatched)
            })
        });
    if provider_operations.is_empty()
        && effect_operations.is_empty()
        && uncommitted_effect_operations.is_empty()
        && unrun_effect_operations.is_empty()
        && process_operations.is_empty()
        && compaction_operations.is_empty()
        && !root_dispatched_without_result
        && !root_without_work
        && !tool_round_completed
        && !decision_unsent
        && fold_row.is_none()
    {
        return Err(refuse(
            "this checkpoint has no unfinished work for a turn to abandon",
        ));
    }
    let command_id = CommandId::new(root_command.command_id())
        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
    let pending_root = code_cell
        .as_ref()
        .filter(|cell| !cell.root_finalized)
        .and_then(|_| {
            ledger.operations().into_iter().find(|record| {
                matches!(record.kind(),
            DurableOperationKind::Command { turn_id: Some(bound) } if *bound == turn_id)
                    && record.request().bytes() == root_command.command_component().bytes()
                    && matches!(record.state(), DurableOperationState::Completed(_))
            })
        });
    let (root_operation, root) = if let Some(record) = pending_root {
        (
            OperationId(record.operation_id()),
            AbandonedRoot::PendingTerminal,
        )
    } else if let Some(operation) = root_operation {
        let prepared = ledger.operations().into_iter().any(|record| {
            record.operation_id() == operation.0
                && matches!(record.state(), DurableOperationState::Prepared)
        });
        (
            operation,
            if prepared {
                AbandonedRoot::Prepared
            } else {
                AbandonedRoot::Dispatched
            },
        )
    } else {
        let terminal = terminal_record_for_command(ledger, root_command)
            .map_err(|_| refuse("the turn's root command is neither running nor terminal"))?;
        if !matches!(
            terminal.outcome,
            DurableTerminalOutcome::RecoveryRequired(_)
        ) {
            return Err(refuse(
                "a turn that delivered a definite result has nothing to abandon",
            ));
        }
        (terminal.operation_id, AbandonedRoot::AlreadyTerminal)
    };
    Ok(AbandonedTurnRecoveryFold {
        turn_id,
        command_id,
        root_operation,
        provider_operations,
        settled_operations,
        uncommitted_effect_operations,
        effect_operations,
        unrun_effect_operations,
        process_operations,
        compaction_operations,
        root,
        tool_round_completed,
    })
}

fn root_is_only_turn_operation(
    ledger: &RuntimeOperationLedger,
    turn_id: TurnId,
    root: OperationId,
) -> bool {
    ledger.operations().into_iter().all(|record| {
        operation_turn_id(record) != Some(turn_id)
            || record.operation_id() == root.0
            || controls::same_turn_control(ledger, record, turn_id)
    })
}

/// The accepted root command that admitted this turn.
fn root_turn_command(
    ledger: &RuntimeOperationLedger,
    turn_id: TurnId,
) -> Result<&DurableCommandRecord, AgentError> {
    ledger
        .commands()
        .into_iter()
        .find(|command| command.turn_id() == Some(turn_id) && is_accepted_root(command))
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))
}

/// The turn the last accepted root command admitted.
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

/// Epoch 25 has no legacy provider records: every round provider record of
/// the turn carries provider-outcome-v2 or tagged closure evidence. Anything
/// else is corrupt and refuses recovery, never reads as "no outcome".
fn require_readable_provider_records(
    ledger: &RuntimeOperationLedger,
    turn_id: TurnId,
) -> Result<(), AgentError> {
    for record in ledger.operations() {
        if operation_turn_id(record) == Some(turn_id) {
            provider_outcome(record)?;
        }
    }
    Ok(())
}

fn terminal_evidence_fold(
    checkpoint: &RuntimeCheckpoint,
) -> Result<LocalTerminalRecoveryFold, AgentError> {
    local_terminal_from_ledger(
        checkpoint,
        checkpoint.manifest().ledger(),
        checkpoint.manifest().lifecycle(),
        false,
    )
}

fn local_terminal_from_ledger(
    checkpoint: &RuntimeCheckpoint,
    ledger: &RuntimeOperationLedger,
    lifecycle: &RuntimeLifecycle,
    allow_uncertain_siblings: bool,
) -> Result<LocalTerminalRecoveryFold, AgentError> {
    let manifest = checkpoint.manifest();
    // Prepared controls leave DispatchEvidence even when the provider round landed.
    // The exact debt checks below still reject unfinished provider or tool work.
    let RuntimeLifecycle::Working {
        root: RuntimeWorkingRoot::Turn { turn_id, .. },
        recovery_phase,
        obligations,
    } = lifecycle
    else {
        return Err(AgentError::new(
            AgentErrorKind::OutcomeUnknown,
            "working checkpoint is outside local terminal finalization scope",
        ));
    };
    require_readable_provider_records(ledger, *turn_id)?;
    let root_command = root_turn_command(ledger, *turn_id)?;
    let root_operation = ledger
        .operations()
        .into_iter()
        .find(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::Command {
                    turn_id: Some(operation_turn)
                } if operation_turn == turn_id
            ) && record.request().bytes() == root_command.command_component().bytes()
        })
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
    if !allow_uncertain_siblings
        && matches!(root_operation.state(), DurableOperationState::Dispatched)
    {
        return landed_terminal_recovery_fold(checkpoint, root_command, root_operation, *turn_id);
    }
    if !allow_uncertain_siblings
        && !matches!(
            recovery_phase,
            WorkingRecoveryPhase::Finalization | WorkingRecoveryPhase::DispatchEvidence
        )
    {
        return Err(AgentError::new(
            AgentErrorKind::OutcomeUnknown,
            "working checkpoint is outside local terminal finalization scope",
        ));
    }
    let root_finalized = matches!(root_operation.state(), DurableOperationState::Finalized(_));
    let root_recorded = root_finalized
        || matches!(
            root_operation.state(),
            DurableOperationState::Completed(_) | DurableOperationState::ProvenNotDispatched(_)
        );
    if allow_uncertain_siblings && !root_recorded {
        return Err(AgentError::new(
            AgentErrorKind::OutcomeUnknown,
            "working checkpoint requires nonlocal recovery evidence",
        ));
    }
    if root_finalized {
        terminal_record_for_command(ledger, root_command)?;
    }
    let mut operations = Vec::with_capacity(obligations.len());
    for obligation in obligations {
        let record = ledger
            .operation(obligation.operation_id())
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
        if controls::same_turn_control(ledger, record, *turn_id)
            && obligation.kind() == RecoveryObligationKind::Command
            && matches!(
                (record.state(), obligation.action()),
                (
                    DurableOperationState::Prepared,
                    RecoveryAction::ProvenNotDispatched
                ) | (
                    DurableOperationState::Dispatched
                        | DurableOperationState::Completed(_)
                        | DurableOperationState::ProvenNotDispatched(_),
                    RecoveryAction::FinalizationRequired
                )
            )
        {
            operations.push(OperationId(record.operation_id()));
            continue;
        }
        if allow_uncertain_siblings
            && root_recorded
            && operation_turn_id(record) == Some(*turn_id)
            && terminal_siblings::uncertain_evidence(record, obligation)?.is_some()
        {
            operations.push(OperationId(record.operation_id()));
            continue;
        }
        if obligation.action() != RecoveryAction::FinalizationRequired {
            return Err(AgentError::new(
                AgentErrorKind::OutcomeUnknown,
                "working checkpoint requires nonlocal recovery evidence",
            ));
        }
        let local_kind = match (record.kind(), obligation.kind()) {
            (DurableOperationKind::Command { .. }, RecoveryObligationKind::Command) => {
                !root_finalized
            }
            (DurableOperationKind::Provider { .. }, RecoveryObligationKind::ProviderAttempt)
            | (
                DurableOperationKind::CompositionEvaluation { .. }
                | DurableOperationKind::CompositionContinuation { .. }
                | DurableOperationKind::CompositionChild { .. },
                RecoveryObligationKind::CodeCell,
            ) => true,
            (DurableOperationKind::Effect { .. }, RecoveryObligationKind::Effect)
            | (
                DurableOperationKind::ProcessSession { .. },
                RecoveryObligationKind::ProcessSession,
            ) => root_finalized,
            _ => false,
        };
        if operation_turn_id(record) != Some(*turn_id)
            || !matches!(
                record.state(),
                DurableOperationState::Completed(_) | DurableOperationState::ProvenNotDispatched(_)
            )
            || !local_kind
        {
            return Err(AgentError::new(
                AgentErrorKind::OutcomeUnknown,
                "working checkpoint lacks exact local finalization evidence",
            ));
        }
        operations.push(OperationId(record.operation_id()));
    }
    if root_finalized {
        // The root already delivered its terminal. Only close exact sibling
        // debt, retaining every record's own evidence and the existing history.
        return Ok(LocalTerminalRecoveryFold {
            synthesized: None,
            turn_id: *turn_id,
            command_id: CommandId::new(root_command.command_id())
                .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?,
            operations,
            output: Vec::new(),
            withhold_prompt: false,
            tail: None,
            family_artifact: manifest.family_artifact().cloned(),
        });
    }
    if !operations.contains(&OperationId(root_operation.operation_id())) {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let (DurableOperationState::Completed(evidence)
    | DurableOperationState::ProvenNotDispatched(evidence)) = root_operation.state()
    else {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    };
    let pending = decode_pending_terminal_record(evidence.bytes())
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
    if pending.command_id != root_command.command_id()
        || pending.turn_id != *turn_id
        || pending.operation_id != OperationId(root_operation.operation_id())
    {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let mut family_artifact = None;
    let output = match &pending.outcome {
        DurableTerminalOutcome::Completed(response) => {
            response
                .validate_for_commit()
                .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
            let matching_providers = ledger
                .operations()
                .into_iter()
                .filter(|record| operations.contains(&OperationId(record.operation_id())))
                .filter(|record| matches!(record.kind(), DurableOperationKind::Provider { .. }))
                // Every provider record of the turn decoded above.
                .filter(|record| {
                    matches!(record.state(), DurableOperationState::Completed(_))
                        && provider_outcome(record).is_ok_and(|outcome| {
                            outcome.is_some_and(|outcome| outcome.response() == Some(response))
                        })
                })
                .collect::<Vec<_>>();
            if matching_providers.len() != 1 {
                return Err(AgentError::new(
                    AgentErrorKind::OutcomeUnknown,
                    "terminal response lacks one exact completed provider result",
                ));
            }
            family_artifact =
                terminal_family_artifact(checkpoint, matching_providers[0], response)?;
            response.output().to_vec()
        }
        DurableTerminalOutcome::Cancelled(_)
        | DurableTerminalOutcome::Failed(_)
        | DurableTerminalOutcome::RecoveryRequired(_) => Vec::new(),
    };
    Ok(LocalTerminalRecoveryFold {
        synthesized: None,
        turn_id: *turn_id,
        command_id: CommandId::new(root_command.command_id())
            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?,
        operations,
        output,
        withhold_prompt: false,
        tail: None,
        family_artifact,
    })
}

/// The finish row (spec 01 §5.5): root R `Dispatched` and T's newest round
/// provider P carrying a recorded `Terminal` or `FailTurn` decision. Recovery
/// runs the consequence commit the live driver would have run over the
/// recorded response, decision and budgets; it measures and classifies
/// nothing. Every other same-turn record settles: with its own evidence, as
/// a control (pending steers unapplied), or, for an uncertain process
/// session, closed as outcome unknown.
fn landed_terminal_recovery_fold(
    checkpoint: &RuntimeCheckpoint,
    command: &DurableCommandRecord,
    root: &DurableOperationRecord,
    turn_id: TurnId,
) -> Result<LocalTerminalRecoveryFold, AgentError> {
    let refusal = || {
        AgentError::new(
            AgentErrorKind::OutcomeUnknown,
            "landed response is outside exact local finalization scope",
        )
    };
    let ledger = checkpoint.manifest().ledger();
    let (provider, recorded) = replay::newest_decision(ledger, turn_id)?.ok_or_else(refusal)?;
    if !matches!(
        provider.state(),
        DurableOperationState::Completed(_)
            | DurableOperationState::ProvenNotDispatched(_)
            | DurableOperationState::OutcomeUnknown(_)
    ) || !matches!(
        recorded.decision,
        RoundDecision::Terminal | RoundDecision::FailTurn(_)
    ) {
        return Err(refusal());
    }
    let records = ledger.operations();
    let mut operations = vec![
        OperationId(root.operation_id()),
        OperationId(provider.operation_id()),
    ];
    for record in &records {
        if operations.contains(&OperationId(record.operation_id()))
            || matches!(record.state(), DurableOperationState::Finalized(_))
        {
            continue;
        }
        if operation_turn_id(record) != Some(turn_id) {
            return Err(refusal());
        }
        let settles = if controls::same_turn_control(ledger, record, turn_id) {
            pending_steering(checkpoint, record, turn_id)
                || matches!(
                    record.state(),
                    DurableOperationState::Completed(_)
                        | DurableOperationState::ProvenNotDispatched(_)
                )
        } else {
            match (record.kind(), record.state()) {
                (
                    DurableOperationKind::Command { .. },
                    DurableOperationState::Completed(_)
                    | DurableOperationState::ProvenNotDispatched(_),
                ) => false,
                (
                    _,
                    DurableOperationState::Completed(_)
                    | DurableOperationState::ProvenNotDispatched(_),
                ) => true,
                (DurableOperationKind::ProcessSession { .. }, state) => {
                    uncertain_process_state(state)
                }
                _ => false,
            }
        };
        if !settles {
            return Err(refusal());
        }
        operations.push(OperationId(record.operation_id()));
    }
    let (outcome, response, tail, family_artifact) = match &recorded.decision {
        RoundDecision::Terminal => {
            let response = recorded.response().cloned().ok_or_else(refusal)?;
            response
                .validate_for_commit()
                .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
            let family_artifact = terminal_family_artifact(checkpoint, provider, &response)?;
            (
                DurableTerminalOutcome::Completed(response.clone()),
                Some(response),
                (false, None),
                family_artifact,
            )
        }
        RoundDecision::FailTurn(shape) => {
            let error = shape.failure.clone().into_error()?;
            let trailing = crate::runtime::driver::trailing_prompt(
                checkpoint.manifest().canonical().history(),
                None,
            );
            let tail = crate::runtime::driver::failure_prompt_tail(
                turn_id,
                &error,
                shape.prompt_withheld,
                trailing,
            )?
            .into_commit()?;
            let family_artifact = if crate::runtime::driver::failure_preserves_artifact(&error) {
                checkpoint.manifest().family_artifact().cloned()
            } else {
                None
            };
            let failure = shape.failure.clone();
            let outcome = match failure.kind {
                AgentErrorKind::Cancelled => DurableTerminalOutcome::Cancelled(failure),
                AgentErrorKind::OutcomeUnknown => DurableTerminalOutcome::RecoveryRequired(failure),
                _ => DurableTerminalOutcome::Failed(failure),
            };
            (outcome, None, tail, family_artifact)
        }
        _ => return Err(refusal()),
    };
    let usage = terminal_turn_usage(&records, turn_id)?;
    let command_id = CommandId::new(command.command_id())
        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
    let operation = OperationId(root.operation_id());
    let evidence = runtime_component(
        RuntimeComponentKind::Operation,
        encode_pending_terminal_record(PendingTerminalResultRecord {
            remedy: match &recorded.decision {
                RoundDecision::FailTurn(shape) => shape.remedy,
                _ => None,
            },
            version: TERMINAL_RESULT_RECORD_VERSION,
            command_id: command_id.as_str().to_owned(),
            turn_id,
            operation_id: operation,
            outcome,
            usage,
        })
        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?,
    )?;
    Ok(LocalTerminalRecoveryFold {
        synthesized: Some((operation, evidence)),
        command_id,
        turn_id,
        operations,
        output: response
            .map(|response| response.output().to_vec())
            .unwrap_or_default(),
        withhold_prompt: tail.0,
        tail: tail.1,
        family_artifact,
    })
}

/// A process session whose outcome no record establishes: recovery closes it
/// as outcome unknown and never signals its orphan.
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

/// The exact usage sum over T's provider outcome records (spec 01 §5.5):
/// each round attempt counts once with its recorded usage, completed and
/// failed alike; a round record without an outcome counts with usage `None`.
/// A compaction's own provider attempt is not a round and is not counted,
/// as the live counters do not count it. Local tool attempts count each
/// effect or cell evaluation with a `Dispatched` transition.
fn terminal_turn_usage(
    records: &[&DurableOperationRecord],
    turn_id: TurnId,
) -> Result<TurnUsage, AgentError> {
    let overflow = || {
        AgentError::new(
            AgentErrorKind::OutcomeUnknown,
            "recovered turn usage overflows its counters",
        )
    };
    let dispatched = |record: &DurableOperationRecord| {
        record
            .transitions()
            .iter()
            .any(|transition| matches!(transition.state(), DurableOperationState::Dispatched))
    };
    let mut usage = TurnUsage::default();
    for record in records
        .iter()
        .filter(|record| operation_turn_id(record) == Some(turn_id))
    {
        match record.kind() {
            DurableOperationKind::Provider {
                root: ModelOperationRoot::Turn(_),
                ..
            } => {
                let outcome = provider_outcome(record)?;
                if !dispatched(record) && outcome.is_none() {
                    continue;
                }
                let reported = outcome.and_then(|outcome| outcome.usage);
                usage
                    .record_model_operation(reported)
                    .map_err(|_| overflow())?;
            }
            DurableOperationKind::Effect { .. }
            | DurableOperationKind::CompositionEvaluation { .. }
                if dispatched(record) =>
            {
                usage.record_local_tool_attempt().map_err(|_| overflow())?;
            }
            _ => {}
        }
    }
    Ok(usage)
}

fn terminal_family_artifact(
    checkpoint: &RuntimeCheckpoint,
    provider: &DurableOperationRecord,
    response: &ModelResponse,
) -> Result<Option<crate::FamilyArtifact>, AgentError> {
    let Some(candidate) = response.family_artifact_candidate() else {
        return Ok(None);
    };
    let prepared: PreparedCallRecord = serde_json::from_slice(provider.request().bytes())
        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
    let canonical = checkpoint.manifest().canonical();
    let source_items = plakous_model::ConversationView::from_shared(
        canonical.history().clone(),
        WorkingProjection::default(),
    )
    .items()
    .len();
    let history = canonical
        .history()
        .append(
            HistorySegment::new(
                history_segment_kind(response.output()),
                response.output().to_vec(),
            )
            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?,
        )
        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
    crate::runtime::family_artifact::promote_family_artifact(
        candidate,
        &prepared,
        &history,
        canonical.context(),
        source_items,
        response.output().len(),
    )
    .map(Some)
    .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))
}

/// The strongest claim the cut supports, which is the one worth printing.
///
/// A running tool outranks an unobserved request, because it is the one that
/// can have changed something outside this session. An unobserved request
/// outranks an unapplied compaction because it is the turn's own work.
pub(in crate::runtime) fn abandoned_turn_claim(
    fold: &AbandonedTurnRecoveryFold,
) -> AbandonedTurnClaim {
    if !fold.effect_operations.is_empty()
        || !fold.process_operations.is_empty()
        || !fold.uncommitted_effect_operations.is_empty()
    {
        AbandonedTurnClaim::RunningTool
    } else if !fold.provider_operations.is_empty() {
        AbandonedTurnClaim::ModelRequest
    } else if !fold.compaction_operations.is_empty() {
        AbandonedTurnClaim::CompactionNotApplied
    } else if fold.tool_round_completed {
        AbandonedTurnClaim::ToolRoundCompleted
    } else {
        AbandonedTurnClaim::NothingStarted
    }
}

pub(in crate::runtime) const fn abandonment_cause(
    fold: &AbandonedTurnRecoveryFold,
) -> AbandonmentCause {
    match fold.root {
        AbandonedRoot::Prepared | AbandonedRoot::Dispatched => AbandonmentCause::ProcessLost,
        AbandonedRoot::AlreadyTerminal | AbandonedRoot::PendingTerminal => {
            AbandonmentCause::Interrupted
        }
    }
}

/// The note for one claim under one cause. Each asserts only its own claim:
/// no billing warning where nothing reached a provider, and no workspace
/// warning where no command could have run.
pub(in crate::runtime) const fn abandoned_turn_note(
    claim: AbandonedTurnClaim,
    cause: AbandonmentCause,
) -> &'static str {
    match (claim, cause) {
        (AbandonedTurnClaim::ToolRoundCompleted, _) => {
            "this turn was abandoned after its tool round completed. The turn was interrupted \
             before the next model request was prepared. The completed tool results remain \
             in this history; they are not retried."
        }
        (AbandonedTurnClaim::RunningTool, AbandonmentCause::ProcessLost) => {
            "this turn was abandoned after the process was lost while a tool was \
             running. The tool's command may have run to completion and changed the \
             workspace, and its result was never observed. No result from it is part \
             of this history, and it is not retried. Check the workspace before \
             relying on what came before this point."
        }
        (AbandonedTurnClaim::RunningTool, AbandonmentCause::Interrupted) => {
            "this turn stopped while a tool or process it started was still running, \
             and its completion could not be confirmed. The command may have run to \
             completion and changed the workspace; its result was never observed. No \
             result from it is part of this history, and it is not retried. Check the \
             workspace before relying on what came before this point."
        }
        (AbandonedTurnClaim::ModelRequest, AbandonmentCause::ProcessLost) => {
            "the model request for this turn was abandoned after its outcome became \
             uncertain: the process was lost before any response was observed. It may \
             have been dispatched and billed. No response from it is part of this \
             history."
        }
        (AbandonedTurnClaim::ModelRequest, AbandonmentCause::Interrupted) => {
            "the model request for this turn was interrupted before its outcome was \
             observed. It may have been processed and billed. No response from it is \
             part of this history, and it is not retried."
        }
        (AbandonedTurnClaim::CompactionNotApplied, AbandonmentCause::ProcessLost) => {
            "this turn was abandoned after the process was lost while the conversation \
             was being compacted. Compaction was not applied; the history before this \
             point is unchanged. The compaction request may have been billed. No \
             response from this turn is part of this history."
        }
        (AbandonedTurnClaim::CompactionNotApplied, AbandonmentCause::Interrupted) => {
            "this turn stopped while the conversation was being compacted. Compaction \
             was not applied; the history before this point is unchanged. The \
             compaction request may have been billed. No response from this turn is \
             part of this history."
        }
        (AbandonedTurnClaim::NothingStarted, AbandonmentCause::ProcessLost) => {
            "this turn was abandoned after the process was lost. Exact evidence \
             proves the tool its last round asked for never started; nothing after \
             that round ran. No result from that round is part of this history."
        }
        (AbandonedTurnClaim::NothingStarted, AbandonmentCause::Interrupted) => {
            "this turn stopped before anything it asked for started, so nothing ran \
             and nothing was changed. No result from this turn is part of this history."
        }
    }
}

/// The one entry a turn abandoned with an unfinished model request leaves.
///
/// The reader of a later turn has to be able to see that the request happened
/// and that nothing came back, or the same prompt simply looks unanswered and
/// the next round repeats it as if nothing had been sent.
///
/// `User` with `Provenance::Instruction` is the product's harness-authored
/// spelling, not a claim that the operator typed this: the pair is what
/// `SessionHistoryItem::is_harness_authored` reads, so a transcript labels the
/// entry `harness` rather than `user`. It is spelled that way because it is the
/// only historical actor every wire format can carry. `System` was truthful and
/// unsendable: the Responses route renders it `role: "system"` inside `input`,
/// which the backend refuses outright with HTTP 400 and
/// `System messages are not allowed`; the Messages route refuses to translate a
/// historical instruction actor at all. A note the next request cannot carry is
/// worse than no note, because it takes the whole session down with it.
fn abandoned_turn_segment(
    turn_id: TurnId,
    claim: AbandonedTurnClaim,
    cause: AbandonmentCause,
) -> Result<HistorySegment, AgentError> {
    abandoned_turn_segment_with_note(turn_id, abandoned_turn_note(claim, cause))
}

fn abandoned_turn_segment_with_note(
    turn_id: TurnId,
    note: &str,
) -> Result<HistorySegment, AgentError> {
    let text = TextBlock::new(note.to_owned(), None)
        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
    let item = ConversationItem::new(
        abandoned_turn_entry_id(turn_id)?,
        Actor::User,
        vec![ContentBlock::Text(text)],
        Provenance::Instruction,
    )
    .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
    HistorySegment::new(HistorySegmentKind::Conversation, vec![item])
        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))
}

/// One stable entry identity per abandoned turn, so a retried publication
/// reproduces the same history rather than a second note.
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
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    EntryId::try_from(uuid::Uuid::from_bytes(bytes)).map_err(|_| {
        AgentError::new(
            AgentErrorKind::Identity,
            "abandoned turn entry identity is invalid",
        )
    })
}

/// The evidence a finalized provider operation carries after abandonment.
///
/// Deliberately not a recovery result: nothing was classified. It records that
/// the outcome was never observed and that the attempt is never to be replayed,
/// which is exactly what is true.
/// The evidence a finalized tool effect carries after abandonment.
///
/// Deliberately stronger than the provider attempt's. A model request that was
/// lost may have cost money; a tool that was lost may have *done* something —
/// written a file, moved a branch, started a process — and there is no way to
/// find out afterwards. The record says the command may have run to completion
/// and that its result was never observed, because both halves are true and the
/// second does not cancel the first.
fn abandoned_effect_evidence() -> Result<ReconstructiveComponent, AgentError> {
    runtime_component(
        RuntimeComponentKind::Operation,
        serde_json::to_vec(&(
            "plakous-agent/abandoned-tool-effect-v1",
            "the process was lost while this tool was running; the command may have \
             run to completion and changed the workspace, its result was never \
             observed or kept in the conversation, any workspace changes remain \
             for review, and it is never replayed",
        ))
        .map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "effect abandonment evidence could not be encoded",
            )
        })?,
    )
}

/// The evidence a joined process session carries after abandonment. Its
/// teardown was never proven, so the process may still be running; this build
/// stores no process identity it could signal.
fn abandoned_process_evidence() -> Result<ReconstructiveComponent, AgentError> {
    abandonment_evidence(
        "plakous-agent/abandoned-process-session-v1",
        "this process session's teardown was never proven; its command may have run to \
         completion and changed the workspace, it may still be running, its result was \
         not kept in the conversation, any workspace changes remain for review, \
         and it is never replayed",
    )
}

/// The evidence a compaction and its provider attempt carry after
/// abandonment. A compaction's effect exists only through its own commit, so
/// one whose response was never applied changed nothing; only its billing is
/// unknown.
fn abandoned_compaction_evidence() -> Result<ReconstructiveComponent, AgentError> {
    abandonment_evidence(
        "plakous-agent/abandoned-compaction-v1",
        "this compaction's response was never applied; the history is unchanged, the \
         request may have been billed, and it is never replayed",
    )
}

fn abandonment_evidence(
    tag: &'static str,
    detail: &'static str,
) -> Result<ReconstructiveComponent, AgentError> {
    runtime_component(
        RuntimeComponentKind::Operation,
        serde_json::to_vec(&(tag, detail)).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "abandonment evidence could not be encoded",
            )
        })?,
    )
}

fn abandoned_operation_evidence() -> Result<ReconstructiveComponent, AgentError> {
    runtime_component(
        RuntimeComponentKind::Operation,
        serde_json::to_vec(&(
            "plakous-agent/abandoned-provider-attempt-v1",
            "the process was lost before this attempt's outcome was observed; it may \
             have been dispatched and billed, no response was kept, and it is never \
             replayed",
        ))
        .map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "abandonment evidence could not be encoded",
            )
        })?,
    )
}

struct RootClosureState {
    history: plakous_model::CanonicalHistory,
    ledger: RuntimeOperationLedger,
    turn_id: TurnId,
    command_id: CommandId,
    family_artifact: Option<crate::FamilyArtifact>,
}

#[allow(
    clippy::too_many_lines,
    reason = "each abandonment debt retains its own evidence and claim"
)]
fn abandonment_closure_state(
    source: &RuntimeCheckpoint,
    fold: AbandonedTurnRecoveryFold,
    claim: AbandonedTurnClaim,
    checkpoint_id: CheckpointId,
) -> Result<RootClosureState, AgentError> {
    let source_manifest = source.manifest();
    let cause = abandonment_cause(&fold);
    let completed_effect_only = !fold.uncommitted_effect_operations.is_empty()
        && fold.effect_operations.is_empty()
        && fold.process_operations.is_empty();
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
    let segment = if let Some(note) = row.and_then(|row| row.note(cause)) {
        abandoned_turn_segment_with_note(fold.turn_id, note)?
    } else if claim == AbandonedTurnClaim::CompactionNotApplied
        && source_manifest
            .ledger()
            .operations()
            .into_iter()
            .any(|record| {
                operation_turn_id(record) == Some(fold.turn_id)
                    && replay::retained_round(record).is_ok_and(|retained| retained)
            })
    {
        abandoned_turn_segment_with_note(
            fold.turn_id,
            match cause {
                AbandonmentCause::ProcessLost => {
                    "this turn was abandoned after the process was lost while the conversation was being compacted. Compaction was not applied and may have been billed. Earlier completed rounds of this turn remain in this history; the history before this point is unchanged."
                }
                AbandonmentCause::Interrupted => {
                    "this turn stopped while the conversation was being compacted. Compaction was not applied and may have been billed. Earlier completed rounds of this turn remain in this history; the history before this point is unchanged."
                }
            },
        )?
    } else if let Some(unsent) = unsent_decision(source, fold.turn_id, fold.root_operation) {
        abandoned_turn_segment_with_note(
            fold.turn_id,
            match unsent {
                replay::UnsentDecision::Retry => {
                    "this turn was abandoned after its model request failed and a retry was decided. The decided retry was not sent and is not retried; the history before this point is unchanged."
                }
                replay::UnsentDecision::Fold => {
                    "this turn was abandoned after the provider refused its request as over the context window. Compaction did not start, and the refused request was not sent again; the history before this point is unchanged."
                }
            },
        )?
    } else if fold.tool_round_completed
        && model_round_boundary(source, fold.turn_id, fold.root_operation)
    {
        abandoned_turn_segment_with_note(
            fold.turn_id,
            "this turn was abandoned after its model round completed, before the next model request was prepared. The completed model response remains in this history; no next request was dispatched or retried.",
        )?
    } else if fold.tool_round_completed && claim == AbandonedTurnClaim::RunningTool {
        abandoned_turn_segment_with_note(
            fold.turn_id,
            "this turn was abandoned after its tool round completed, before the next model \
     request was prepared. Completed tool results remain in this history. A process \
     it started was still running; its outcome is unknown, it may have changed the \
     workspace, and it is not retried. Check the workspace before carrying on.",
        )?
    } else if claim == AbandonedTurnClaim::RunningTool && completed_effect_only {
        abandoned_turn_segment_with_note(
            fold.turn_id,
            "this turn was abandoned after a tool finished, but its result was not added \
             to the conversation. It may have changed the workspace and is not retried. \
             Check the workspace before relying on what came before this point.",
        )?
    } else if claim == AbandonedTurnClaim::NothingStarted
        && code_cell_abandonment_fold(source).is_ok_and(|cell| cell.turn_abandonment)
    {
        abandoned_turn_segment_with_note(
            fold.turn_id,
            "this turn's remaining Code Cell work was abandoned before dispatch. The abandoned \
     operations never started and are not retried; completed work retains its own \
     evidence. No cell result is inferred from this abandonment.",
        )?
    } else if claim == AbandonedTurnClaim::NothingStarted
        && root_is_only_turn_operation(source_manifest.ledger(), fold.turn_id, fold.root_operation)
    {
        abandoned_turn_segment_with_note(
            fold.turn_id,
            "this turn was abandoned after its process was lost, before any model request \
     or tool started. Nothing ran, no result was inferred, and the prompt is not retried.",
        )?
    } else {
        abandoned_turn_segment(fold.turn_id, claim, cause)?
    };
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
    // Preserve completed provider/process evidence and proven-unrun effects.
    // Rewriting them as unknown would lose facts; none strengthens the note.
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
            ledger = finalize_not_started(&ledger, record, checkpoint_id, history.identity())?;
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
    if fold.root == AbandonedRoot::Prepared {
        let record = ledger
            .operation(fold.root_operation.0)
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
        ledger = finalize_not_started(&ledger, record, checkpoint_id, history.identity())?;
    }
    if fold.root == AbandonedRoot::Dispatched {
        let cancelled = DurableAgentError::from_error(&AgentError::new(
            AgentErrorKind::Session,
            row.and_then(fold_rows::FoldRow::cancellation).unwrap_or(match claim {
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
                    source_manifest.ledger(),
                    fold.turn_id,
                    fold.root_operation,
                ) =>
                {
                    "the turn was abandoned after the process was lost, before any work started"
                }
                _ => {
                    "the turn was abandoned after the process was lost; the tool its last round asked for never started"
                }
            }),
        ))?;
        let pending = encode_pending_terminal_record(PendingTerminalResultRecord {
            remedy: None,
            version: TERMINAL_RESULT_RECORD_VERSION,
            command_id: fold.command_id.as_str().to_owned(),
            turn_id: fold.turn_id,
            operation_id: fold.root_operation,
            outcome: DurableTerminalOutcome::Cancelled(cancelled),
            usage: terminal_turn_usage(&source_manifest.ledger().operations(), fold.turn_id)?,
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
            history.identity(),
        )?;
        ledger = ledger
            .transition_operation(
                fold.root_operation.0,
                DurableOperationTransition::new(
                    checkpoint_id,
                    DurableOperationState::Finalized(finalized),
                ),
            )
            .map_err(runtime_manifest_error)?;
    }
    if fold.root == AbandonedRoot::PendingTerminal {
        let evidence = ledger
            .operation(fold.root_operation.0)
            .and_then(|record| match record.state() {
                DurableOperationState::Completed(evidence) => Some(evidence),
                _ => None,
            })
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
        ledger = ledger
            .transition_operation(
                fold.root_operation.0,
                DurableOperationTransition::new(
                    checkpoint_id,
                    DurableOperationState::Finalized(finalized_operation_evidence(
                        evidence,
                        checkpoint_id,
                        history.identity(),
                    )?),
                ),
            )
            .map_err(runtime_manifest_error)?;
    }
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

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the recovery publisher consumes the exact immutable source/store tuple"
)]
pub(in crate::runtime) async fn recover_root_turn_publication(
    store: &Arc<dyn SessionStore>,
    lease: &BranchLease,
    head: BranchHead,
    source: &Arc<RuntimeCheckpoint>,
    source_bundle: &CheckpointObjectBundle,
    closure: RootClosure,
    permitted: &[RecoveryObligation],
    timeout: Duration,
    checkpoint_id: CheckpointId,
    durability_telemetry: Option<&DurabilityCommitTelemetry>,
) -> Result<(Arc<RuntimeCheckpoint>, CheckpointObjectBundle, BranchHead), AgentError> {
    let source_manifest = source.manifest();
    if permitted != source_manifest.lifecycle().obligations() {
        return Err(AgentError::new(
            AgentErrorKind::Session,
            "recovery permit disagrees with the exact checkpoint obligations",
        ));
    }
    let report = crate::runtime::checkpoint::owner::root_closure_report(source, &closure)?;
    let RootClosureState {
        history,
        mut ledger,
        turn_id,
        command_id,
        family_artifact,
    } = match closure {
        RootClosure::Abandon { fold, claim } => {
            abandonment_closure_state(source, fold, claim, checkpoint_id)?
        }
        RootClosure::FinalizeRecorded {
            terminal,
            code_cell,
        } => terminal_closure_state(source, terminal, code_cell.is_some(), checkpoint_id)?,
        RootClosure::Complete(terminal) => {
            terminal_closure_state(source, terminal, false, checkpoint_id)?
        }
    };
    if let Some(report) = report {
        let commands = ledger.commands().into_iter().cloned().collect();
        let mut records = Vec::new();
        for record in ledger.operations() {
            let mut transitions = record.transitions().to_vec();
            if matches!(record.kind(), DurableOperationKind::Command { turn_id: Some(bound) } if *bound == turn_id)
                && let Some(last) = transitions.last()
                && let DurableOperationState::Finalized(evidence) = last.state()
                && let Ok(mut terminal) =
                    crate::runtime::terminal::read_terminal_result_record(evidence.bytes())
                && terminal.command_id == command_id.as_str()
                && terminal.turn_id == turn_id
            {
                terminal.abandonment = Some(report.clone());
                let evidence = runtime_component(
                    RuntimeComponentKind::Operation,
                    crate::runtime::terminal::encode_terminal_result_record(&terminal)
                        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?,
                )?;
                let transition = DurableOperationTransition::new(
                    checkpoint_id,
                    DurableOperationState::Finalized(evidence),
                );
                if last.checkpoint_id() == checkpoint_id {
                    let last = transitions
                        .last_mut()
                        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
                    *last = transition;
                } else {
                    transitions.push(transition);
                }
            }
            records.push(
                DurableOperationRecord::new(
                    record.operation_id(),
                    record.kind().clone(),
                    record.request().clone(),
                    transitions,
                )
                .map_err(runtime_manifest_error)?,
            );
        }
        ledger = RuntimeOperationLedger::new(commands, records).map_err(runtime_manifest_error)?;
    }
    if !matches!(
        working_lifecycle(turn_id, &ledger)?,
        RuntimeLifecycle::Quiescent
    ) {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let branch_revision = source_manifest
        .topology()
        .branch_revision
        .checked_add(1)
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
    if branch_revision != head.revision().get().saturating_add(1) {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let context = source_manifest
        .canonical()
        .context()
        .retained_for_exact_resume();
    let lifecycle = RuntimeLifecycle::Quiescent;
    let commit_identity = checkpoint_commit_identity(CheckpointCommitFacts {
        checkpoint_id,
        parent_checkpoint_id: source.id(),
        branch_revision,
        lifecycle: &lifecycle,
        history_identity: history.identity(),
        ledger: &ledger,
        context_identity: context.identity(),
        family_artifact: family_artifact.as_ref(),
    })?;
    let canonical = RuntimeCanonicalState::new(
        source_manifest.canonical().stable_prefix().clone(),
        history,
        WorkingProjection::default(),
        context,
    )
    .map_err(runtime_manifest_error)?;
    let checkpoint = Arc::new(
        RuntimeCheckpoint::new(
            RuntimeManifest::new(
                source_manifest.provenance().clone(),
                RuntimeTopology {
                    session_id: source_manifest.topology().session_id,
                    branch_id: source_manifest.topology().branch_id,
                    checkpoint_id,
                    parent_checkpoint_id: Some(source.id()),
                    branch_revision,
                    fork: source_manifest.topology().fork,
                    lease_fence: lease.fence().get(),
                    commit_identity,
                },
                lifecycle,
                source_manifest.harness_plan_component().clone(),
                source_manifest.provider_configuration().clone(),
                canonical,
                source_manifest.binding().clone(),
                source_manifest.tools().clone(),
                ledger,
                family_artifact,
                source_manifest.interpreter_observation().cloned(),
                source_manifest.jobs().clone(),
                source_manifest.durability(),
            )
            .map_err(runtime_manifest_error)?,
        )
        .map_err(runtime_manifest_error)?,
    );
    terminal_record_at_checkpoint(&checkpoint, &command_id)?;
    let bundle = RuntimeCheckpointCodec::advance(&checkpoint, source_bundle)
        .map_err(checkpoint_bundle_error)?;
    let request = CommitCheckpoint::new(lease.clone(), head, bundle.clone())
        .with_admission(StoreAdmission::Recovery)
        .with_feed_facts(crate::feed::facts::commit_facts(source, &checkpoint)?);
    let expected =
        CheckpointObjectRef::from_store(checkpoint_id, bundle.root(), checkpoint.integrity());
    let receipt = Box::pin(commit_with_acknowledgement_recovery(
        store,
        timeout,
        checkpoint_id,
        request,
        head,
        expected,
        checkpoint.manifest().topology().commit_identity,
        durability_telemetry,
    ))
    .await?;
    Ok((checkpoint, bundle, receipt.head()))
}

fn finalize_not_started(
    ledger: &RuntimeOperationLedger,
    record: &DurableOperationRecord,
    checkpoint_id: CheckpointId,
    history_identity: Digest,
) -> Result<RuntimeOperationLedger, AgentError> {
    let (evidence, finalized) = if let DurableOperationKind::Command {
        turn_id: Some(turn_id),
    } = record.kind()
    {
        let command = root_turn_command(ledger, *turn_id)?;
        let evidence = runtime_component(
            RuntimeComponentKind::Operation,
            encode_pending_terminal_record(PendingTerminalResultRecord {
                    remedy: None,
                    version: TERMINAL_RESULT_RECORD_VERSION,
                command_id: command.command_id().to_owned(),
                turn_id: *turn_id,
                operation_id: OperationId(record.operation_id()),
                outcome: DurableTerminalOutcome::Cancelled(DurableAgentError::from_error(
                    &AgentError::new(
                        AgentErrorKind::Session,
                        if root_is_only_turn_operation(
                            ledger,
                            *turn_id,
                            OperationId(record.operation_id()),
                        ) {
                            "the turn was abandoned after the process was lost, before any work started"
                        } else {
                            "the accepted root was abandoned before dispatch"
                        },
                    ),
                )?),
                usage: TurnUsage::default(),
            })
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "queued follow-up terminal record could not be encoded",
                )
            })?,
        )?;
        let finalized = finalized_operation_evidence(&evidence, checkpoint_id, history_identity)?;
        (evidence, finalized)
    } else {
        let evidence = abandonment_evidence(
            "plakous-agent/abandoned-tool-effect-v1",
            "this Code Cell operation was proven not dispatched and is never replayed",
        )?;
        (evidence.clone(), evidence)
    };
    ledger
        .transition_operation_many(
            record.operation_id(),
            [
                DurableOperationTransition::new(
                    checkpoint_id,
                    DurableOperationState::ProvenNotDispatched(evidence),
                ),
                DurableOperationTransition::new(
                    checkpoint_id,
                    DurableOperationState::Finalized(finalized),
                ),
            ],
        )
        .map_err(runtime_manifest_error)
}

/// Records that an operation's outcome was never observed and closes it, in
/// that order and in one checkpoint, so a later reader sees both facts.
fn close_as_unobserved(
    ledger: &RuntimeOperationLedger,
    operation: OperationId,
    evidence: &ReconstructiveComponent,
    checkpoint_id: CheckpointId,
) -> Result<RuntimeOperationLedger, AgentError> {
    if let Some(ledger) =
        in_turn_compaction::finalize_completed_fold(ledger, operation, checkpoint_id)?
    {
        return Ok(ledger);
    }
    let already_unknown = matches!(
        ledger
            .operation(operation.0)
            .map(DurableOperationRecord::state),
        Some(DurableOperationState::OutcomeUnknown(_))
    );
    let unknown = (!already_unknown).then(|| {
        DurableOperationTransition::new(
            checkpoint_id,
            DurableOperationState::OutcomeUnknown(evidence.clone()),
        )
    });
    let finalized = DurableOperationTransition::new(
        checkpoint_id,
        DurableOperationState::Finalized(evidence.clone()),
    );
    ledger
        .transition_operation_many(operation.0, unknown.into_iter().chain([finalized]))
        .map_err(runtime_manifest_error)
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
