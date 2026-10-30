use super::{
    AgentError, AgentErrorKind, CommandId, DurableCommandRecord, DurableOperationKind,
    DurableOperationRecord, DurableOperationState, DurableTerminalOutcome, HistorySegment,
    LocalTerminalRecoveryFold, ModelOperationRoot, ModelResponse, OperationId,
    PendingTerminalResultRecord, PreparedCallRecord, RecoveryAction, RecoveryObligationKind,
    RoundDecision, RuntimeCheckpoint, RuntimeComponentKind, RuntimeLifecycle,
    RuntimeOperationLedger, RuntimeWorkingRoot, SessionStoreError, TERMINAL_RESULT_RECORD_VERSION,
    TurnId, TurnUsage, WorkingProjection, WorkingRecoveryPhase, controls,
    decode_pending_terminal_record, encode_pending_terminal_record, history_segment_kind,
    operation_turn_id, pending_steering, provider_outcome, replay,
    require_readable_provider_records, root_turn_command, runtime_component,
    terminal_record_for_command, terminal_siblings, uncertain_process_state,
};

pub(super) fn terminal_evidence_fold(
    checkpoint: &RuntimeCheckpoint,
) -> Result<LocalTerminalRecoveryFold, AgentError> {
    local_terminal_from_ledger(
        checkpoint,
        checkpoint.manifest().ledger(),
        checkpoint.manifest().lifecycle(),
        false,
    )
}

#[allow(
    clippy::too_many_lines,
    reason = "Local terminal reconstruction verifies prompt disposition and all command and operation closures before returning a result"
)]
pub(super) fn local_terminal_from_ledger(
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
#[allow(
    clippy::too_many_lines,
    reason = "Landed terminal recovery binds committed history and usage to the exact recorded root terminal"
)]
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

/// The exact usage sum over T's provider outcome records (spec 01 §5.5):
/// each round attempt counts once with its recorded usage, completed and
/// failed alike; a round record without an outcome counts with usage `None`.
/// A compaction's own provider attempt is not a round and is not counted,
/// as the live counters do not count it. Local tool attempts count each
/// effect or cell evaluation with a `Dispatched` transition.
pub(super) fn terminal_turn_usage(
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
