use super::{
    AbandonedRoot, AbandonedTurnClaim, AbandonedTurnRecoveryFold, AbandonmentCause, Actor,
    AgentError, AgentErrorKind, CheckpointId, CommandId, ContentBlock, ConversationItem, Digest,
    DurableAgentError, DurableCodeCellAbandonmentDisposition, DurableCommandRecord,
    DurableOperationKind, DurableOperationRecord, DurableOperationState,
    DurableOperationTransition, DurableTerminalOutcome, HistorySegment, HistorySegmentKind,
    ModelOperationRoot, OperationId, PendingTerminalResultRecord, Provenance,
    ReconstructiveComponent, RecoveryAction, RecoveryObligationKind, RuntimeCheckpoint,
    RuntimeComponentKind, RuntimeLifecycle, RuntimeOperationLedger, RuntimeWorkingRoot,
    SessionStoreError, TERMINAL_RESULT_RECORD_VERSION, TextBlock, TurnFoldFacts, TurnId, TurnRow,
    TurnUsage, abandoned_turn_entry_id, abandonment_rows, code_cell_abandonment_fold, controls,
    encode_pending_terminal_record, finalized_operation_evidence, fold_rows, in_turn_compaction,
    is_accepted_root, last_root_turn, operation_turn_id, pending_steering, provider_outcome,
    queued_root, replay, runtime_component, runtime_manifest_error, terminal_record_for_command,
};
use crate::runtime::continuation::{RestoredContinuationFrame, decode_restored_continuation};

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
pub(super) fn abandonment_evidence_fold(
    checkpoint: &RuntimeCheckpoint,
    barrier: super::DecisionBarrier,
) -> Result<AbandonedTurnRecoveryFold, AgentError> {
    abandonment_fold(checkpoint, barrier, None)
}

/// [`abandonment_evidence_fold`] as a live cancel reads it: the same rows
/// over the live head, with `closing` the restored hosted baton the cancel's
/// terminal finalizes (a tool batch or a server pause, promoted or not).
#[allow(
    clippy::too_many_lines,
    reason = "Turn abandonment folds recorded provider and effect evidence without inventing successful outcomes for unresolved work"
)]
pub(super) fn abandonment_fold(
    checkpoint: &RuntimeCheckpoint,
    barrier: super::DecisionBarrier,
    closing: Option<OperationId>,
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
    // A barrier this open closes is the one decision its restorable frame
    // names; any other decision record keeps its own rule.
    // So is a promoted server pause, whose response is already in history: its
    // baton is finalized as a refusal finalizes it live. A tool batch, or a
    // pause the lost owner never promoted, keeps its own rule and restores.
    let (closed_decision, closed_continuation) = match barrier {
        super::DecisionBarrier::Close(_) => match decode_restored_continuation(checkpoint) {
            Ok(RestoredContinuationFrame::EffectDecision(frame)) => {
                (Some(frame.decision_operation.0), None)
            }
            Ok(RestoredContinuationFrame::ServerPause(frame)) if frame.promoted => {
                (None, Some(frame.continuation_operation.0))
            }
            _ => (None, None),
        },
        super::DecisionBarrier::Restore => (None, None),
    };
    let closed_continuation = closing.map(|baton| baton.0).or(closed_continuation);
    let facts = TurnFoldFacts {
        ledger,
        turn_id,
        code_cell: code_cell.is_some(),
        root_dispatched,
        batch_pending,
        closed_decision,
        closed_continuation,
    };
    let mut barrier_closed = false;
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
        let operation = OperationId(record.operation_id());
        match abandonment_rows::turn_row(&facts, obligation, record, root_operation.is_none()) {
            Some(TurnRow::Root) => root_operation = Some(operation),
            Some(TurnRow::Retained) => {}
            Some(TurnRow::Settled) => {
                barrier_closed |= closed_continuation == Some(operation.0);
                settled_operations.push(operation);
            }
            Some(TurnRow::CompletedUnadded) => {
                settled_operations.push(operation);
                uncommitted_effect_operations.push(operation);
            }
            Some(TurnRow::ProviderUncertain) => provider_operations.push(operation),
            Some(TurnRow::EffectUncertain) => effect_operations.push(operation),
            Some(TurnRow::EffectUnrun) => {
                barrier_closed |= closed_decision == Some(operation.0);
                unrun_effect_operations.push(operation);
            }
            Some(TurnRow::ProcessUncertain) => process_operations.push(operation),
            None => {
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
        (true, Some(operation)) => replay::closing_round_boundary(
            ledger,
            turn_id,
            operation,
            closed_continuation.map(OperationId),
        )?
        .is_some(),
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
                        || closed_continuation == Some(operation.0)
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
        closed_continuation: closed_continuation
            .filter(|_| barrier_closed)
            .map(OperationId),
        barrier_cause: match barrier {
            super::DecisionBarrier::Close(cause) if barrier_closed => Some(cause),
            _ => None,
        },
    })
}

pub(super) fn root_is_only_turn_operation(
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
pub(super) fn root_turn_command(
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
/// Epoch 25 has no legacy provider records: every round provider record of
/// the turn carries provider-outcome-v2 or tagged closure evidence. Anything
/// else is corrupt and refuses recovery, never reads as "no outcome".
pub(super) fn require_readable_provider_records(
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
pub(super) fn abandoned_turn_segment(
    turn_id: TurnId,
    claim: AbandonedTurnClaim,
    cause: AbandonmentCause,
) -> Result<HistorySegment, AgentError> {
    abandoned_turn_segment_with_note(turn_id, abandoned_turn_note(claim, cause))
}

pub(super) fn abandoned_turn_segment_with_note(
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
pub(super) fn abandoned_effect_evidence() -> Result<ReconstructiveComponent, AgentError> {
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
pub(super) fn abandoned_process_evidence() -> Result<ReconstructiveComponent, AgentError> {
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
pub(super) fn abandoned_compaction_evidence() -> Result<ReconstructiveComponent, AgentError> {
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

pub(super) fn abandoned_operation_evidence() -> Result<ReconstructiveComponent, AgentError> {
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

pub(super) fn finalize_not_started(
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
pub(super) fn close_as_unobserved(
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

pub(in crate::runtime) const fn abandonment_cause(
    fold: &AbandonedTurnRecoveryFold,
) -> AbandonmentCause {
    if let Some(cause) = fold.barrier_cause {
        return cause;
    }
    match fold.root {
        AbandonedRoot::Prepared | AbandonedRoot::Dispatched => AbandonmentCause::ProcessLost,
        AbandonedRoot::AlreadyTerminal | AbandonedRoot::PendingTerminal => {
            AbandonmentCause::Interrupted
        }
    }
}

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
