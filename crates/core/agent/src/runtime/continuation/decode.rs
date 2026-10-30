use super::{
    AgentError, AgentErrorKind, AwaitEffectDecisionV1, ContinuationBindingReference,
    ContinuationToolSlot, DurableCommandDisposition, DurableOperationKind, DurableOperationState,
    FinishReason, ModelOperationRoot, OperationId, RecoveryAction, RestoredContinuationFrame,
    RestoredHostedFrame, RoundDecision, RuntimeCheckpoint, RuntimeContinuation, RuntimeLifecycle,
    RuntimeWorkingRoot, SessionStoreError, WorkingProjection, WorkingRecoveryPhase,
    collect_local_calls, provider_outcome, reconcile_hosted_tool_slots, restore_frame_plan,
    validate_hosted_frame,
};

#[allow(
    clippy::too_many_lines,
    reason = "Effect-decision decoding checks the persisted grant, call slot, provider round, and working-root bindings together"
)]
pub(in crate::runtime) fn decode_await_effect_decision_continuation(
    checkpoint: &RuntimeCheckpoint,
) -> Result<AwaitEffectDecisionV1, AgentError> {
    let manifest = checkpoint.manifest();
    let RuntimeLifecycle::Working {
        root: RuntimeWorkingRoot::Turn { turn_id, .. },
        recovery_phase: WorkingRecoveryPhase::EffectDecision,
        obligations,
    } = manifest.lifecycle()
    else {
        return Err(AgentError::new(
            AgentErrorKind::OutcomeUnknown,
            "checkpoint is not an effect-decision continuation barrier",
        ));
    };
    let decision_obligations = obligations
        .iter()
        .filter(|obligation| obligation.action() == RecoveryAction::RestoreEffectDecision)
        .collect::<Vec<_>>();
    if decision_obligations.len() != 1 {
        return Err(AgentError::new(
            AgentErrorKind::OutcomeUnknown,
            "checkpoint has missing or mixed effect-decision continuation authority",
        ));
    }
    let decision_record = manifest
        .ledger()
        .operation(decision_obligations[0].operation_id())
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
    let DurableOperationState::DecisionRequired(pending) = decision_record.state() else {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    };
    if pending.dispatch_capability_minted() || pending.request() != decision_record.request() {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let continuation: RuntimeContinuation = serde_json::from_slice(pending.request().bytes())
        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
    if serde_json::to_vec(&continuation)
        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?
        != pending.request().bytes()
    {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let RuntimeContinuation::AwaitEffectDecisionV1(frame) = continuation else {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    };
    let frame = *frame;
    let pending_index = usize::try_from(frame.pending_index)
        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
    if frame.dispatch_capability_minted
        || frame.root.turn_id != *turn_id
        || frame.decision.turn_id() != *turn_id
        || frame.decision.attempt_id() != frame.attempt_id
        || frame.decision.binding_digest()
            != match frame.slots.get(pending_index) {
                Some(ContinuationToolSlot::PendingDecision { binding_digest, .. }) => {
                    *binding_digest
                }
                _ => return Err(AgentError::from_store(SessionStoreError::corrupt())),
            }
        || frame.decision_operation.0 != decision_record.operation_id()
        || !matches!(
            decision_record.kind(),
            DurableOperationKind::EffectDecision {
                turn_id: decision_turn,
                attempt_id,
                call_id,
            } if *decision_turn == *turn_id
                && *attempt_id == frame.attempt_id
                && *call_id == frame.decision.call_id()
        )
        || frame.provider_response.plan_digest() != frame.provider_plan_digest
        || frame.working != WorkingProjection::default()
        || frame.model_admission_reference
            != ContinuationBindingReference::from(manifest.binding().admission().reference())
        || frame.policy_reference != ContinuationBindingReference::from(manifest.tools().policy())
    {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let root_command = manifest
        .ledger()
        .commands()
        .into_iter()
        .find(|record| record.command_id() == frame.root.command_id)
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
    let root_operation = manifest
        .ledger()
        .operation(frame.root.operation_id.0)
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
    if root_command.turn_id() != Some(*turn_id)
        || root_command.binding() != frame.root.binding
        || root_command.disposition() != DurableCommandDisposition::Accepted
        || !matches!(
            root_operation.kind(),
            DurableOperationKind::Command {
                turn_id: Some(operation_turn)
            } if *operation_turn == *turn_id
        )
        || !matches!(root_operation.state(), DurableOperationState::Dispatched)
        || root_operation.request().bytes() != root_command.command_component().bytes()
    {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    restore_frame_plan(
        manifest.provider_configuration(),
        manifest,
        frame.provider_operation,
        &frame.provider_request,
        frame.provider_plan_digest,
    )?;
    if frame.provider_response.validate_for_commit().is_err() {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let provider_record = manifest
        .ledger()
        .operation(frame.provider_operation.0)
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
    if !matches!(
        provider_record.kind(),
        DurableOperationKind::Provider {
            root: ModelOperationRoot::Turn(provider_turn),
            attempt_id,
        } if *provider_turn == *turn_id && *attempt_id == frame.attempt_id
    ) || !matches!(provider_record.state(), DurableOperationState::Completed(_))
        || !provider_outcome(provider_record)?.is_some_and(|outcome| {
            outcome.response() == Some(&frame.provider_response)
                && outcome.decision == RoundDecision::ToolBatch
        })
    {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    for (index, slot) in frame.slots.iter().enumerate() {
        match slot {
            ContinuationToolSlot::Completed {
                call, operation_id, ..
            } if index < pending_index => {
                let operation = manifest
                    .ledger()
                    .operation(operation_id.0)
                    .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
                if !matches!(
                    operation.kind(),
                    DurableOperationKind::Effect {
                        turn_id: operation_turn,
                        attempt_id,
                        call_id,
                        source_index,
                    } if *operation_turn == *turn_id
                        && *attempt_id == frame.attempt_id
                        && *call_id == call.call_id()
                        && usize::try_from(*source_index).ok() == Some(index)
                ) || !matches!(
                    operation.state(),
                    DurableOperationState::Completed(_)
                        | DurableOperationState::ProvenNotDispatched(_)
                ) {
                    return Err(AgentError::from_store(SessionStoreError::corrupt()));
                }
            }
            ContinuationToolSlot::PendingDecision { call, .. } if index == pending_index => {
                if call.call_id() != frame.decision.call_id() {
                    return Err(AgentError::from_store(SessionStoreError::corrupt()));
                }
            }
            ContinuationToolSlot::NotAdmitted { .. } if index > pending_index => {}
            _ => return Err(AgentError::from_store(SessionStoreError::corrupt())),
        }
    }
    if let Some(operation) = frame.hosted_continuation_operation {
        let hosted = manifest
            .ledger()
            .operation(operation.0)
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
        if !matches!(
            (hosted.kind(), hosted.state()),
            (
                DurableOperationKind::ToolBatchContinuation {
                    turn_id: hosted_turn,
                    attempt_id,
                },
                DurableOperationState::ContinuationRequired(component)
            ) if *hosted_turn == *turn_id
                && *attempt_id == frame.attempt_id
                && component == hosted.request()
        ) {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
    }
    Ok(frame)
}

pub(in crate::runtime) fn decode_restored_continuation(
    checkpoint: &RuntimeCheckpoint,
) -> Result<RestoredContinuationFrame, AgentError> {
    let RuntimeLifecycle::Working { recovery_phase, .. } = checkpoint.manifest().lifecycle() else {
        return Err(AgentError::new(
            AgentErrorKind::OutcomeUnknown,
            "checkpoint is not a working continuation barrier",
        ));
    };
    match recovery_phase {
        WorkingRecoveryPhase::EffectDecision => {
            decode_await_effect_decision_continuation(checkpoint)
                .map(Box::new)
                .map(RestoredContinuationFrame::EffectDecision)
        }
        WorkingRecoveryPhase::HostedContinuation => decode_hosted_continuation(checkpoint),
        _ => Err(AgentError::new(
            AgentErrorKind::OutcomeUnknown,
            "working checkpoint has no certified continuation decoder",
        )),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "Hosted continuation decoding verifies the exact barrier and provider evidence before restoring tool-batch or server-pause state"
)]
pub(in crate::runtime) fn decode_hosted_continuation(
    checkpoint: &RuntimeCheckpoint,
) -> Result<RestoredContinuationFrame, AgentError> {
    let manifest = checkpoint.manifest();
    let RuntimeLifecycle::Working {
        root: RuntimeWorkingRoot::Turn { turn_id, .. },
        recovery_phase: WorkingRecoveryPhase::HostedContinuation,
        obligations,
    } = manifest.lifecycle()
    else {
        return Err(AgentError::new(
            AgentErrorKind::OutcomeUnknown,
            "checkpoint is not a hosted continuation barrier",
        ));
    };
    let hosted = obligations
        .iter()
        .filter(|obligation| {
            matches!(
                obligation.action(),
                RecoveryAction::RestoreToolBatch | RecoveryAction::RestoreServerPause
            )
        })
        .collect::<Vec<_>>();
    if hosted.len() != 1 {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let record = manifest
        .ledger()
        .operation(hosted[0].operation_id())
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
    let DurableOperationState::ContinuationRequired(component) = record.state() else {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    };
    if component != record.request() {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let continuation: RuntimeContinuation = serde_json::from_slice(component.bytes())
        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
    if serde_json::to_vec(&continuation)
        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?
        != component.bytes()
    {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let operation = OperationId(record.operation_id());
    match (hosted[0].action(), record.kind(), continuation) {
        (
            RecoveryAction::RestoreServerPause,
            DurableOperationKind::ServerPauseContinuation {
                turn_id: operation_turn,
                attempt_id,
            },
            RuntimeContinuation::AwaitServerPauseV1(frame),
        ) if operation_turn == turn_id && *attempt_id == frame.attempt_id => {
            let promoted = validate_hosted_frame(
                checkpoint,
                &frame.root,
                &frame.provider_request,
                frame.provider_plan_digest,
                &frame.provider_response,
                frame.attempt_id,
                frame.provider_operation,
                &RoundDecision::Continue,
                &frame.working,
                &frame.model_admission_reference,
                &frame.policy_reference,
            )?;
            if frame.provider_response.finish().reason() != FinishReason::ProviderContinuation
                || frame
                    .provider_response
                    .family_artifact_candidate()
                    .is_none()
                || !collect_local_calls(
                    &frame.provider_response,
                    &manifest.canonical().projection(),
                )
                .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?
                .is_empty()
            {
                return Err(AgentError::from_store(SessionStoreError::corrupt()));
            }
            let remaining_observation_budget = frame.observation_budget;
            Ok(RestoredContinuationFrame::ServerPause(Box::new(
                RestoredHostedFrame {
                    frame: *frame,
                    continuation_operation: operation,
                    remaining_observation_budget,
                    promoted,
                    recovered_dispatched: 0,
                    observation_clips: Vec::new(),
                },
            )))
        }
        (
            RecoveryAction::RestoreToolBatch,
            DurableOperationKind::ToolBatchContinuation {
                turn_id: operation_turn,
                attempt_id,
            },
            RuntimeContinuation::AwaitToolBatchV2(frame),
        ) if operation_turn == turn_id && *attempt_id == frame.attempt_id => {
            let promoted = validate_hosted_frame(
                checkpoint,
                &frame.root,
                &frame.provider_request,
                frame.provider_plan_digest,
                &frame.provider_response,
                frame.attempt_id,
                frame.provider_operation,
                &RoundDecision::ToolBatch,
                &frame.working,
                &frame.model_admission_reference,
                &frame.policy_reference,
            )?;
            let (frame, remaining_observation_budget, recovered_dispatched, observation_clips) =
                reconcile_hosted_tool_slots(checkpoint, *frame, promoted)?;
            Ok(RestoredContinuationFrame::ToolBatch(Box::new(
                RestoredHostedFrame {
                    frame,
                    continuation_operation: operation,
                    remaining_observation_budget,
                    promoted,
                    recovered_dispatched,
                    observation_clips,
                },
            )))
        }
        _ => Err(AgentError::from_store(SessionStoreError::corrupt())),
    }
}
