//! Persisted continuation codecs and exact runtime reconstruction.

use crate::local_composition::LocalCompositionConstruction;
use crate::runtime::build::stable_runtime_environment_component;
use crate::runtime::config::{AgentConfig, RUNTIME_COMPONENT_CODEC_REVISION};
use crate::runtime::control::{actor_command_effect, control_error_from_durable};
use crate::runtime::driver::execution::canonicalize_observed_tool_execution;
use crate::runtime::observation::ObservationClip;
use crate::runtime::operation_record::{
    ProviderOutcome, RoundDecision, TurnBudgets, collect_local_calls, provider_outcome,
    recorded_provider_response,
};
use crate::runtime::terminal::{
    DurableTerminalResultRecord, TerminalRecordRejection, read_terminal_result_record,
};
use crate::runtime::{
    CommandLedgerEntry, DurableCommandEnvelope, OperationId, ToolExecutionResult, ToolPresentation,
};
use crate::tool_runtime::ToolRuntime;
use crate::{
    ActorCommandEffect, AgentError, AgentErrorKind, BindingReference, CommandAck, CommandId,
    DurableCommandDisposition, DurableCommandRecord, DurableOperationKind, DurableOperationRecord,
    DurableOperationState, EffectDecisionRef, FamilyArtifact, ProfileError,
    ReconstructiveComponent, RecoveryAction, RuntimeCheckpoint, RuntimeComponentKind,
    RuntimeLifecycle, RuntimeManifest, RuntimeOperationLedger, RuntimeRevalidationWitness,
    RuntimeWorkingRoot, SessionStoreError, TurnUsage, WorkingRecoveryPhase,
};
use plakous_model::{
    Actor, AttemptId, CompiledProviderConfiguration, ConversationItem, Digest, DigestDomain,
    EntryId, FinishReason, LanguageModel, ModelOperationRoot, ModelRequest, ModelResponse,
    PreparedCallRecord, Provenance, RuntimeModelAuthority, Timestamp, ToolCallBlock, TurnId,
    ValidatedRequestPlan, ValidationMode, WorkingProjection,
};
use plakous_tools::{
    FrozenToolExposureDescriptor, FrozenToolProjectionDescriptorV2, FrozenToolRegistry,
    FrozenToolRegistryDescriptor, TOOL_EXPOSURE_DESCRIPTOR_CODEC_REVISION,
    TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION, TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION,
    TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION, ToolBindingError, ToolExecutorBindings,
    WorkspacePolicy,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;
pub(super) mod command_feed;
pub(super) fn decode_await_effect_decision_continuation(
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

/// Rebuilds the plan a continuation frame's provider response was validated
/// under, from the prepared call that dispatched it.
///
/// A plan digest binds the freshness-sensitive descriptor binding, and a
/// re-resolution or a fresh reopen observes the descriptor afresh: neither the
/// checkpoint's configuration (kept from creation) nor a running driver's can
/// recompute it. The frame is bound instead to the provider operation's
/// recorded prepared call: its plan digest and request must be the recorded
/// ones, and the record's authority evidence must carry the plan's descriptor
/// binding. `configuration` (the checkpoint's, or the driver's) is checked only
/// through what does not age: the selected and effective identities, which
/// carry the semantic descriptor binding, the account and credential
/// authority, and the descriptor's semantic axes and capability decisions.
pub(in crate::runtime) fn restore_frame_plan(
    configuration: &CompiledProviderConfiguration,
    manifest: &RuntimeManifest,
    provider_operation: OperationId,
    provider_request: &ModelRequest,
    provider_plan_digest: Digest,
) -> Result<ValidatedRequestPlan, AgentError> {
    let corrupt = || AgentError::from_store(SessionStoreError::corrupt());
    let record = manifest
        .ledger()
        .operation(provider_operation.0)
        .ok_or_else(corrupt)?;
    if !matches!(record.kind(), DurableOperationKind::Provider { .. }) {
        return Err(corrupt());
    }
    let prepared: PreparedCallRecord =
        serde_json::from_slice(record.request().bytes()).map_err(|_| corrupt())?;
    let projection = prepared.projection();
    let evidence = prepared.authority_evidence();
    let stable = configuration.effective().authority();
    if projection.plan() != provider_plan_digest
        || projection.selected_provider_configuration_id() != configuration.selected_identity()
        || projection.effective_provider_configuration_id() != configuration.effective_identity()
        || evidence.account_binding() != stable.account_binding()
        || evidence.credential_authority_binding() != stable.credential_authority_binding()
    {
        return Err(corrupt());
    }
    let plan = prepared
        .restore_plan(
            configuration.effective().descriptor(),
            provider_request.clone(),
            ValidationMode::Strict,
        )
        .map_err(|_| corrupt())?;
    if plan.digest() != provider_plan_digest || plan.request() != provider_request {
        return Err(corrupt());
    }
    Ok(plan)
}

pub(super) fn decode_restored_continuation(
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

pub(super) fn decode_hosted_continuation(
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

#[allow(
    clippy::too_many_arguments,
    reason = "the hosted frame binding is one exact reconstruction tuple"
)]
fn validate_hosted_frame(
    checkpoint: &RuntimeCheckpoint,
    root: &ContinuationRoot,
    provider_request: &ModelRequest,
    provider_plan_digest: Digest,
    provider_response: &ModelResponse,
    attempt_id: AttemptId,
    provider_operation: OperationId,
    decision: &RoundDecision,
    working: &WorkingProjection,
    model_admission_reference: &ContinuationBindingReference,
    policy_reference: &ContinuationBindingReference,
) -> Result<bool, AgentError> {
    let manifest = checkpoint.manifest();
    let RuntimeLifecycle::Working {
        root: RuntimeWorkingRoot::Turn { turn_id, .. },
        ..
    } = manifest.lifecycle()
    else {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    };
    if root.turn_id != *turn_id
        || provider_response.plan_digest() != provider_plan_digest
        || working != &WorkingProjection::default()
        || *model_admission_reference
            != ContinuationBindingReference::from(manifest.binding().admission().reference())
        || *policy_reference != ContinuationBindingReference::from(manifest.tools().policy())
    {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let command = manifest
        .ledger()
        .commands()
        .into_iter()
        .find(|record| record.command_id() == root.command_id)
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
    let root_operation = manifest
        .ledger()
        .operation(root.operation_id.0)
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
    if command.turn_id() != Some(*turn_id)
        || command.binding() != root.binding
        || command.disposition() != DurableCommandDisposition::Accepted
        || !matches!(root_operation.kind(), DurableOperationKind::Command { turn_id: Some(id) } if id == turn_id)
        || !matches!(root_operation.state(), DurableOperationState::Dispatched)
        || root_operation.request().bytes() != command.command_component().bytes()
    {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let plan = restore_frame_plan(
        manifest.provider_configuration(),
        manifest,
        provider_operation,
        provider_request,
        provider_plan_digest,
    )?;
    if provider_response.validate_for_commit().is_err() {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let provider = manifest
        .ledger()
        .operation(provider_operation.0)
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
    if !matches!(provider.kind(), DurableOperationKind::Provider { root: ModelOperationRoot::Turn(id), attempt_id: candidate } if id == turn_id && *candidate == attempt_id)
    {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    // A record that does not decode is typed integrity, never "no frame".
    let records_frame = |evidence: &ReconstructiveComponent| -> Result<bool, AgentError> {
        Ok(ProviderOutcome::decode(evidence)?.is_some_and(|outcome| {
            outcome.response() == Some(provider_response) && outcome.decision == *decision
        }))
    };
    let mut completed_frame = false;
    for transition in provider.transitions() {
        if let DurableOperationState::Completed(evidence) = transition.state() {
            completed_frame |= records_frame(evidence)?;
        }
    }
    let promoted = match provider.state() {
        DurableOperationState::Completed(evidence) if records_frame(evidence)? => false,
        DurableOperationState::Finalized(_) if completed_frame => true,
        _ => return Err(AgentError::from_store(SessionStoreError::corrupt())),
    };
    if promoted {
        let projection = manifest.canonical().projection();
        let committed = projection.items();
        let start = plan.request().input().items().len();
        let end = start
            .checked_add(provider_response.output().len())
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
        if end > committed.len()
            || committed
                .iter()
                .skip(start)
                .take(provider_response.output().len())
                .ne(provider_response.output())
        {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
        match provider_response.family_artifact_candidate() {
            Some(candidate) => {
                let covered = projection
                    .canonical_item_prefix_identity(end)
                    .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                candidate
                    .validate_for_commit(&plan, covered, provider_response.output().len())
                    .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                let expected =
                    FamilyArtifact::from_candidate(candidate).map_err(runtime_manifest_error)?;
                if manifest.family_artifact() != Some(&expected) {
                    return Err(AgentError::from_store(SessionStoreError::corrupt()));
                }
            }
            None if manifest.family_artifact().is_none() => {}
            None => return Err(AgentError::from_store(SessionStoreError::corrupt())),
        }
    }
    Ok(promoted)
}

/// One clipped observation recovered while reconciling a restored tool batch.
#[derive(Clone)]
pub(in crate::runtime) struct RestoredObservationClip {
    /// Tool whose observation was truncated.
    pub(in crate::runtime) name: plakous_model::ToolName,
    /// Call whose observation was truncated.
    pub(in crate::runtime) call_id: plakous_model::CallId,
    /// What the budget kept and removed.
    pub(in crate::runtime) clip: ObservationClip,
}

fn reconcile_hosted_tool_slots(
    checkpoint: &RuntimeCheckpoint,
    mut frame: AwaitToolBatchV2,
    promoted: bool,
) -> Result<
    (
        AwaitToolBatchV2,
        crate::runtime::observation::budget::ObservationBudget,
        usize,
        Vec<RestoredObservationClip>,
    ),
    AgentError,
> {
    let manifest = checkpoint.manifest();
    let canonical = manifest.canonical().projection();
    let prior = if promoted {
        frame.provider_request.input()
    } else {
        &canonical
    };
    let calls = collect_local_calls(&frame.provider_response, prior)
        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    if calls.len() != frame.slots.len()
        || calls.iter().zip(&frame.slots).any(|(call, slot)| {
            let stored = match slot {
                ContinuationToolSlot::Completed { call, .. }
                | ContinuationToolSlot::PendingDecision { call, .. }
                | ContinuationToolSlot::NotAdmitted { call, .. } => call,
            };
            call != stored
        })
    {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    if !calls.iter().any(|call| {
        call.correlation()
            .caller_lineage()
            .provider_programmatic_kind()
            .is_some()
    }) {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let mut effects = BTreeMap::new();
    let mut resolved_decisions = BTreeSet::new();
    for record in manifest.ledger().operations() {
        match record.kind() {
            DurableOperationKind::Effect {
                turn_id,
                attempt_id,
                call_id,
                source_index,
            } if *turn_id == frame.root.turn_id && *attempt_id == frame.attempt_id => {
                let index = usize::try_from(*source_index)
                    .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                if calls.get(index).map(ToolCallBlock::call_id) != Some(*call_id)
                    || effects.insert(index, record).is_some()
                {
                    return Err(AgentError::from_store(SessionStoreError::corrupt()));
                }
            }
            DurableOperationKind::EffectDecision {
                turn_id,
                attempt_id,
                call_id,
            } if *turn_id == frame.root.turn_id && *attempt_id == frame.attempt_id => {
                let index = calls
                    .iter()
                    .position(|call| call.call_id() == *call_id)
                    .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
                if !matches!(record.state(), DurableOperationState::DecisionRequired(_))
                    && !resolved_decisions.insert(index)
                {
                    return Err(AgentError::from_store(SessionStoreError::corrupt()));
                }
            }
            _ => {}
        }
    }
    let mut remaining_budget = frame.observation_budget;
    let mut clips = Vec::new();
    let mut dispatched = 0usize;
    let mut saw_gap = false;
    for (index, slot) in frame.slots.iter_mut().enumerate() {
        let (call, result_entry_id) = match slot {
            ContinuationToolSlot::NotAdmitted {
                call,
                result_entry_id,
            } => (call.clone(), *result_entry_id),
            _ => return Err(AgentError::from_store(SessionStoreError::corrupt())),
        };
        let effect = effects.remove(&index);
        let Some(effect) = effect else {
            if resolved_decisions.contains(&index) {
                return Err(AgentError::new(
                    AgentErrorKind::OutcomeUnknown,
                    "resolved hosted effect decision lacks an exact effect operation",
                ));
            }
            saw_gap = true;
            continue;
        };
        if saw_gap {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
        let (evidence, was_dispatched) = match effect.state() {
            DurableOperationState::Completed(evidence) => (evidence, true),
            DurableOperationState::ProvenNotDispatched(evidence) => (evidence, false),
            DurableOperationState::Finalized(_) => effect
                .transitions()
                .iter()
                .rev()
                .find_map(|transition| match transition.state() {
                    DurableOperationState::Completed(evidence) => Some((evidence, true)),
                    DurableOperationState::ProvenNotDispatched(evidence) => Some((evidence, false)),
                    _ => None,
                })
                .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?,
            _ => return Err(AgentError::from_store(SessionStoreError::corrupt())),
        };
        dispatched = dispatched.saturating_add(usize::from(was_dispatched));
        let (tag, evidence_entry, execution): (String, EntryId, ToolExecutionResult) =
            serde_json::from_slice(evidence.bytes())
                .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
        if tag != "completed-tool-outcome-v1" || evidence_entry != result_entry_id {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
        let (item, clip) =
            canonicalize_observed_tool_execution(result_entry_id, execution, &mut remaining_budget)
                .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
        if let Some(clip) = clip {
            clips.push(RestoredObservationClip {
                name: call.exposed_name().clone(),
                call_id: call.call_id(),
                clip,
            });
        }
        *slot = ContinuationToolSlot::Completed {
            call,
            result_entry_id,
            item,
            operation_id: OperationId(effect.operation_id()),
        };
    }
    if !effects.is_empty() {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    Ok((frame, remaining_budget, dispatched, clips))
}

#[allow(
    clippy::too_many_arguments,
    reason = "reopen must compare the complete live reconstruction authority tuple"
)]
pub(super) fn runtime_revalidation_witness(
    _model: &dyn LanguageModel,
    provider_configuration: &CompiledProviderConfiguration,
    tools: &ToolRuntime,
    presentation: &ToolPresentation,
    workspace: &WorkspacePolicy,
    local_composition: Option<&LocalCompositionConstruction>,
    admission: crate::RevalidationFact,
    checked_at: Timestamp,
) -> Result<RuntimeRevalidationWitness, AgentError> {
    let descriptor = provider_configuration.effective().descriptor();
    let semantic_descriptor_binding = descriptor.semantic_binding_digest().map_err(|_| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "resolved model descriptor could not be revalidated",
        )
    })?;
    let account = admission.reference().clone();
    let profile = BindingReference::new("model-adapter", descriptor.profile().as_str(), 1)
        .map_err(runtime_manifest_error)?;
    let registry_descriptor = tools.registry_descriptor().map_err(tool_descriptor_error)?;
    let (registry_revision, registry_bytes, exposure_revision, exposure_bytes) = match presentation
    {
        ToolPresentation::V1(exposure) => (
            TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION,
            registry_descriptor
                .encode_v1()
                .map_err(tool_descriptor_error)?,
            TOOL_EXPOSURE_DESCRIPTOR_CODEC_REVISION,
            FrozenToolExposureDescriptor::capture(exposure)
                .and_then(|descriptor| descriptor.encode_v1())
                .map_err(tool_descriptor_error)?,
        ),
        ToolPresentation::V2(projection) => (
            TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION,
            registry_descriptor
                .encode_v2()
                .map_err(tool_descriptor_error)?,
            TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION,
            FrozenToolProjectionDescriptorV2::capture(projection)
                .and_then(|descriptor| descriptor.encode_v2())
                .map_err(tool_descriptor_error)?,
        ),
    };
    let registry = runtime_component_revision(
        RuntimeComponentKind::ToolRegistry,
        registry_revision,
        registry_bytes,
    )?;
    let exposure = runtime_component_revision(
        RuntimeComponentKind::ToolExposure,
        exposure_revision,
        exposure_bytes,
    )?;
    let environment = stable_runtime_environment_component(
        workspace,
        local_composition,
        provider_configuration,
        crate::runtime::build::live_job_config(tools)?,
    )?;
    let policy_revision = tools.policy_revision();
    let effect_policy = runtime_component(
        RuntimeComponentKind::EffectPolicy,
        format!(
            "{}:{}",
            policy_revision.policy_id(),
            policy_revision.revision()
        )
        .into_bytes(),
    )?;
    let policy = BindingReference::new(
        "tool-policy",
        policy_revision.policy_id(),
        policy_revision.revision(),
    )
    .map_err(runtime_manifest_error)?;
    Ok(RuntimeRevalidationWitness::new(
        checked_at,
        account,
        profile,
        semantic_descriptor_binding,
        semantic_descriptor_binding,
        admission,
        registry.integrity(),
        exposure.integrity(),
        environment.integrity(),
        effect_policy.integrity(),
        policy,
    ))
}

pub(super) fn rehydrate_runtime_indexes(
    authority: &RuntimeCheckpoint,
    feed_sequences: &BTreeMap<CommandId, crate::feed::FeedSeq>,
    durable: bool,
) -> Result<(BTreeMap<CommandId, CommandLedgerEntry>, u64, u64), AgentError> {
    let ledger = authority.manifest().ledger();
    let active_turn = match authority.manifest().lifecycle() {
        RuntimeLifecycle::Working {
            root: RuntimeWorkingRoot::Turn { turn_id, .. },
            ..
        } => Some(*turn_id),
        _ => None,
    };
    let mut commands = BTreeMap::new();
    let mut next_command = 1_u64;
    for record in ledger.commands() {
        next_command = next_command.max(record.sequence().checked_add(1).ok_or_else(|| {
            AgentError::new(
                AgentErrorKind::Session,
                "durable command sequence space was exhausted",
            )
        })?);
        let command_id = CommandId::new(record.command_id()).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Session,
                "durable command identity could not be reconstructed",
            )
        })?;
        let envelope: DurableCommandEnvelope =
            serde_json::from_slice(record.command_component().bytes()).map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Session,
                    "durable command receipt bytes could not be reconstructed",
                )
            })?;
        let reconstructed = Digest::sha256(DigestDomain::Blob, 1, &[&envelope.canonical_input])
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Session,
                    "durable command binding could not be reconstructed",
                )
            })?;
        if envelope.version != 1 || reconstructed != record.binding() {
            return Err(AgentError::new(
                AgentErrorKind::Session,
                "durable command binding is inconsistent",
            ));
        }
        let effect = actor_command_effect(&envelope.effect)?;
        let (acknowledgement, effect) =
            match (record.disposition(), effect, envelope.rejection.as_ref()) {
                (DurableCommandDisposition::Accepted, ActorCommandEffect::Rejected, None) => {
                    return Err(AgentError::new(
                        AgentErrorKind::Session,
                        "accepted durable command carries a rejection effect",
                    ));
                }
                (DurableCommandDisposition::Accepted, effect, None) => (
                    CommandAck::accepted(command_id.clone(), record.sequence(), record.turn_id()),
                    effect,
                ),
                (
                    DurableCommandDisposition::Rejected,
                    ActorCommandEffect::Rejected,
                    Some(rejection),
                ) => (
                    CommandAck::rejected(
                        command_id.clone(),
                        record.sequence(),
                        record.turn_id(),
                        control_error_from_durable(rejection)?,
                    ),
                    ActorCommandEffect::Rejected,
                ),
                _ => {
                    return Err(AgentError::new(
                        AgentErrorKind::Session,
                        "durable command disposition disagrees with its exact receipt",
                    ));
                }
            };
        // Queued follow-up roots have receipts but no terminal yet; the reopen's
        // abandonment publisher closes them before ordinary admission resumes.
        let queued_followup = active_turn.is_some_and(|turn_id| {
            ledger.operations().into_iter().any(|operation| {
                operation.request().bytes() == record.command_component().bytes()
                    && crate::runtime::durable::composition_recovery::queued_root(
                        authority, operation, turn_id,
                    )
            })
        });
        let terminal = if matches!(effect, ActorCommandEffect::Root)
            && (record.turn_id() == active_turn || queued_followup)
        {
            None
        } else if matches!(effect, ActorCommandEffect::Root) {
            Some(terminal_record_for_command(ledger, record)?)
        } else {
            None
        };
        commands.insert(
            command_id.clone(),
            CommandLedgerEntry {
                binding: record.binding(),
                acknowledgement: acknowledgement
                    .with_feed_seq(feed_sequences.get(&command_id).copied()),
                admission_reclaimed: durable && !feed_sequences.contains_key(&command_id),
                effect,
                result: None,
                terminal,
            },
        );
    }
    let next_operation = ledger
        .operations()
        .into_iter()
        .map(DurableOperationRecord::operation_id)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| {
            AgentError::new(
                AgentErrorKind::Session,
                "durable operation identity space was exhausted",
            )
        })?;
    Ok((commands, next_command, next_operation))
}

pub(super) fn terminal_record_for_command(
    ledger: &RuntimeOperationLedger,
    command: &DurableCommandRecord,
) -> Result<DurableTerminalResultRecord, AgentError> {
    let turn_id = command.turn_id().ok_or_else(|| {
        AgentError::new(
            AgentErrorKind::Session,
            "accepted root command omitted its durable turn binding",
        )
    })?;
    // A record that is present but re-encodes differently is a different
    // failure from one that is not there, and the refusal has to say which.
    // Reported as `None` alone, the 2026-09-18 resume blocker read as a
    // missing record when the record was intact and one optional field had
    // changed shape.
    let mut rejection = None;
    let terminal = ledger
        .operations()
        .into_iter()
        .filter(|operation| {
            matches!(
                operation.kind(),
                DurableOperationKind::Command {
                    turn_id: Some(operation_turn)
                } if *operation_turn == turn_id
            ) && operation.request().bytes() == command.command_component().bytes()
        })
        .find_map(|operation| match operation.state() {
            DurableOperationState::Finalized(evidence) => {
                match read_terminal_result_record(evidence.bytes()) {
                    Ok(terminal) if terminal.operation_id.0 == operation.operation_id() => {
                        Some(terminal)
                    }
                    Ok(_) => None,
                    Err(error) => {
                        // A round-trip failure is the diagnosable one, so it
                        // outranks an "absent" from some other operation.
                        if rejection.is_none()
                            || matches!(error, TerminalRecordRejection::RoundTrip(_))
                        {
                            rejection = Some(error);
                        }
                        None
                    }
                }
            }
            _ => None,
        })
        .ok_or_else(|| {
            // A record this build re-encodes differently was written by an
            // incompatible build; an absent one is a damaged session.
            let kind = if matches!(rejection, Some(TerminalRecordRejection::RoundTrip(_))) {
                AgentErrorKind::IncompatibleSession
            } else {
                AgentErrorKind::Session
            };
            AgentError::new(
                kind,
                format!(
                    "accepted root command has {}",
                    rejection.map_or_else(
                        || TerminalRecordRejection::Absent.detail(),
                        |rejection| rejection.detail(),
                    )
                ),
            )
        })?;
    if terminal.command_id != command.command_id() || terminal.turn_id != turn_id {
        return Err(AgentError::new(
            AgentErrorKind::Session,
            "terminal result record disagrees with its root command binding",
        ));
    }
    Ok(terminal)
}

pub(super) fn terminal_record_at_checkpoint(
    checkpoint: &RuntimeCheckpoint,
    command_id: &CommandId,
) -> Result<DurableTerminalResultRecord, AgentError> {
    let ledger = checkpoint.manifest().ledger();
    let command = ledger
        .commands()
        .into_iter()
        .find(|command| command.command_id() == command_id.as_str())
        .ok_or_else(|| {
            AgentError::new(
                AgentErrorKind::Session,
                "terminal checkpoint omitted its admitted root command",
            )
        })?;
    terminal_record_for_command(ledger, command)
}

pub(super) fn runtime_component(
    kind: RuntimeComponentKind,
    bytes: Vec<u8>,
) -> Result<ReconstructiveComponent, AgentError> {
    ReconstructiveComponent::new(kind, RUNTIME_COMPONENT_CODEC_REVISION, bytes)
        .map_err(runtime_manifest_error)
}

pub(super) fn runtime_component_revision(
    kind: RuntimeComponentKind,
    revision: u32,
    bytes: Vec<u8>,
) -> Result<ReconstructiveComponent, AgentError> {
    ReconstructiveComponent::new(kind, revision, bytes).map_err(runtime_manifest_error)
}

pub(super) fn reconstruct_persisted_tools(
    manifest: &RuntimeManifest,
    bindings: &ToolExecutorBindings,
) -> Result<(FrozenToolRegistry, ToolPresentation), AgentError> {
    let tools = manifest.tools();
    let registry_revision = tools.registry().codec_revision();
    let exposure_revision = tools.exposure().codec_revision();
    if !matches!(
        (registry_revision, exposure_revision),
        (
            TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION,
            TOOL_EXPOSURE_DESCRIPTOR_CODEC_REVISION
        ) | (
            TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION,
            TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION
        )
    ) {
        return Err(AgentError::new(
            AgentErrorKind::Session,
            "durable tools contain unsupported or mixed V1/V2 state",
        ));
    }
    let registry_descriptor = match registry_revision {
        TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION => {
            FrozenToolRegistryDescriptor::decode_v1(tools.registry().bytes())
        }
        TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION => {
            FrozenToolRegistryDescriptor::decode_v2(tools.registry().bytes())
        }
        _ => unreachable!("closed revision pair validated above"),
    }
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Session,
            "durable tool registry is unsupported or corrupt",
        )
    })?;
    let registry = registry_descriptor
        .resolve(bindings)
        .map_err(|error| match error {
            ToolBindingError::MissingBinding { id } | ToolBindingError::DuplicateBinding { id } => {
                AgentError::new(
                    AgentErrorKind::ExternalBindingUnavailable,
                    format!("a required tool executor binding is unavailable: {id}"),
                )
            }
            ToolBindingError::BindingIdentityMismatch { id, .. } => AgentError::new(
                AgentErrorKind::ExternalBindingUnavailable,
                format!("a required tool executor binding has a different identity: {id}"),
            ),
            ToolBindingError::Descriptor(_) => AgentError::new(
                AgentErrorKind::Session,
                "durable tool registry failed exact reconstruction",
            ),
            _ => AgentError::new(
                AgentErrorKind::Session,
                "durable tool registry binding failed closed",
            ),
        })?;
    let presentation = match exposure_revision {
        TOOL_EXPOSURE_DESCRIPTOR_CODEC_REVISION => {
            FrozenToolExposureDescriptor::decode_v1(tools.exposure().bytes(), &registry_descriptor)
                .and_then(|descriptor| descriptor.resolve(registry.clone()))
                .map(ToolPresentation::V1)
        }
        TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION => FrozenToolProjectionDescriptorV2::decode_v2(
            tools.exposure().bytes(),
            &registry_descriptor,
        )
        .and_then(|descriptor| descriptor.resolve(registry.clone()))
        .map(Arc::new)
        .map(ToolPresentation::V2),
        _ => unreachable!("closed revision pair validated above"),
    }
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Session,
            "durable tool presentation failed exact reconstruction",
        )
    })?;
    Ok((registry, presentation))
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "map_err supplies the owned semantic validation failure"
)]
pub(super) fn tool_descriptor_error(error: plakous_tools::ToolDescriptorError) -> AgentError {
    AgentError::new(
        AgentErrorKind::InvalidInput,
        format!("tool semantics are not reconstruction-complete: {error}"),
    )
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "map_err supplies the owned semantic validation failure"
)]
pub(super) fn runtime_manifest_error(error: crate::RuntimeManifestError) -> AgentError {
    AgentError::new(
        AgentErrorKind::Internal,
        format!("runtime manifest construction or validation failed: {error}"),
    )
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "map_err supplies the owned semantic validation failure"
)]
pub(super) fn checkpoint_bundle_error(error: crate::CheckpointBundleError) -> AgentError {
    let kind = if error == crate::CheckpointBundleError::UnsupportedRevision {
        AgentErrorKind::IncompatibleSession
    } else {
        AgentErrorKind::Session
    };
    AgentError::new(
        kind,
        format!("runtime checkpoint object mapping failed closed: {error}"),
    )
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "map_err supplies the owned rehydration failure"
)]
pub(super) fn runtime_rehydration_error(error: crate::RuntimeRehydrationError) -> AgentError {
    // An account, profile or admission mismatch is a revalidation failure of
    // the caller's binding. Everything else means this build cannot read or
    // rebuild what an earlier build stored.
    let kind = match error {
        crate::RuntimeRehydrationError::Revalidation(
            crate::recovery_scope::RevalidationError::BindingMismatch,
        ) => AgentErrorKind::InvalidInput,
        _ => AgentErrorKind::IncompatibleSession,
    };
    // The typed source names the exact recovery scope, binding, or manifest
    // fact that did not revalidate; without it a reopen refusal is undiagnosable.
    AgentError::new(
        kind,
        format!("runtime checkpoint recovery scope or revalidation failed: {error}"),
    )
}

pub(super) fn profile_construction_error(error: &ProfileError) -> AgentError {
    AgentError::new(
        AgentErrorKind::InvalidInput,
        format!("harness profile construction failed: {error}"),
    )
}

impl RestoredContinuationFrame {
    pub(super) const fn root(&self) -> &ContinuationRoot {
        match self {
            Self::EffectDecision(frame) => &frame.root,
            Self::ToolBatch(frame) => &frame.frame.root,
            Self::ServerPause(frame) => &frame.frame.root,
        }
    }

    pub(super) const fn config(&self) -> DurableAgentConfig {
        match self {
            Self::EffectDecision(frame) => frame.config,
            Self::ToolBatch(frame) => frame.frame.config,
            Self::ServerPause(frame) => frame.frame.config,
        }
    }

    pub(super) const fn usage(&self) -> TurnUsage {
        match self {
            Self::EffectDecision(frame) => frame.usage,
            Self::ToolBatch(frame) => frame.frame.usage,
            Self::ServerPause(frame) => frame.frame.usage,
        }
    }

    pub(super) const fn budgets(&self) -> TurnBudgets {
        match self {
            Self::EffectDecision(frame) => frame.budgets,
            Self::ToolBatch(frame) => frame.frame.budgets,
            Self::ServerPause(frame) => frame.frame.budgets,
        }
    }

    pub(super) fn request_anchor(&self) -> Option<Digest> {
        match self {
            Self::EffectDecision(frame) => frame.request_anchor,
            Self::ToolBatch(frame) => frame.frame.request_anchor,
            Self::ServerPause(frame) => frame.frame.request_anchor,
        }
    }

    pub(super) const fn context_anchor(&self) -> Option<plakous_model::ContextAnchor> {
        match self {
            Self::EffectDecision(frame) => frame.context_anchor,
            Self::ToolBatch(frame) => frame.frame.context_anchor,
            Self::ServerPause(frame) => frame.frame.context_anchor,
        }
    }

    pub(super) const fn provider_request(&self) -> &ModelRequest {
        match self {
            Self::EffectDecision(frame) => &frame.provider_request,
            Self::ToolBatch(frame) => &frame.frame.provider_request,
            Self::ServerPause(frame) => &frame.frame.provider_request,
        }
    }

    pub(super) fn steering(&self) -> &[ContinuationSteering] {
        match self {
            Self::EffectDecision(frame) => &frame.steering,
            Self::ToolBatch(frame) => &frame.frame.steering,
            Self::ServerPause(frame) => &frame.frame.steering,
        }
    }

    pub(super) fn pending_decision(&self) -> Option<EffectDecisionRef> {
        match self {
            Self::EffectDecision(frame) => Some(frame.decision.clone()),
            Self::ToolBatch(_) | Self::ServerPause(_) => None,
        }
    }
}

impl From<&BindingReference> for ContinuationBindingReference {
    fn from(reference: &BindingReference) -> Self {
        Self {
            authority: reference.authority().to_owned(),
            subject: reference.subject().to_owned(),
            revision: reference.revision(),
        }
    }
}

impl DurableAgentConfig {
    pub(super) fn from_config(config: AgentConfig) -> Result<Self, AgentError> {
        fn nanos(value: Duration) -> Result<u64, AgentError> {
            u64::try_from(value.as_nanos()).map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "runtime duration exceeded the continuation codec bound",
                )
            })
        }
        Ok(Self {
            prompt_queue_capacity: config.prompt_queue_capacity,
            control_queue_capacity: config.control_queue_capacity,
            steering_queue_capacity: config.steering_queue_capacity,
            follow_up_queue_capacity: config.follow_up_queue_capacity,
            event_capacity: config.event_capacity,
            event_byte_capacity: config.event_byte_capacity,
            max_event_subscribers: config.max_event_subscribers,
            tool_progress_capacity: config.tool_progress_capacity,
            effect_decision_timeout_nanos: nanos(config.effect_decision_timeout)?,
            model_cleanup_timeout_nanos: nanos(config.model_cleanup_timeout)?,
            model_operation_timeout_nanos: nanos(config.model_operation_timeout)?,
        })
    }
}

#[cfg(test)]
pub(super) fn previous_response_metadata(
    ledger: &RuntimeOperationLedger,
    plan: &ValidatedRequestPlan,
    authority: &RuntimeModelAuthority,
) -> Option<plakous_model::ModelResponseMetadata> {
    previous_response(ledger, plan, authority)
        .ok()
        .flatten()
        .map(|(metadata, _)| metadata)
}

/// The runtime-proven provider response whose output is the newest assistant
/// item of `plan`: its metadata and its provider response id.
///
/// # Errors
///
/// A provider record whose outcome does not decode is typed integrity.
pub(super) fn previous_response(
    ledger: &RuntimeOperationLedger,
    plan: &ValidatedRequestPlan,
    authority: &RuntimeModelAuthority,
) -> Result<Option<(plakous_model::ModelResponseMetadata, Option<String>)>, AgentError> {
    for record in ledger.operations() {
        if matches!(
            record.state(),
            DurableOperationState::Completed(_) | DurableOperationState::Finalized(_)
        ) {
            recorded_provider_response(record)?;
        }
    }
    Ok(proven_previous_response(ledger, plan, authority))
}

/// [`previous_response`] over a ledger whose provider records all decode.
fn proven_previous_response(
    ledger: &RuntimeOperationLedger,
    plan: &ValidatedRequestPlan,
    authority: &RuntimeModelAuthority,
) -> Option<(plakous_model::ModelResponseMetadata, Option<String>)> {
    if plan.binding_digest() != authority.descriptor_binding() {
        return None;
    }
    let latest = plan
        .request()
        .input()
        .items()
        .into_iter()
        .filter(|item| item.actor() == Actor::Assistant)
        .last()?;
    if latest.provenance() != Provenance::ModelOutput {
        return None;
    }
    let mut matched = None;
    for record in ledger.operations() {
        if !matches!(record.kind(), DurableOperationKind::Provider { .. }) {
            continue;
        }
        if !matches!(
            record.state(),
            DurableOperationState::Completed(_) | DurableOperationState::Finalized(_)
        ) {
            continue;
        }
        let Some(response) = recorded_provider_response(record).ok().flatten() else {
            continue;
        };
        if !response
            .output()
            .iter()
            .any(|item| item.id() == latest.id())
        {
            continue;
        }
        // Two operations claiming the same canonical identity are not provenance.
        if matched.is_some() {
            return None;
        }
        let prepared: PreparedCallRecord = serde_json::from_slice(record.request().bytes()).ok()?;
        let source = prepared.authority_evidence();
        // The sealed effective configuration binds the stable descriptor semantics.
        // Comparing historical freshness timestamps would lose attribution on reopen;
        // the current plan still requires its exact fresh descriptor above.
        if response.plan_digest() != prepared.projection().plan()
            || source.account_binding() != authority.account_binding()
            || source.credential_authority_binding() != authority.credential_authority_binding()
            || prepared.projection().selected_provider_configuration_id()
                != plan.request().selected_provider_configuration_id()
            || prepared.projection().effective_provider_configuration_id()
                != plan.request().effective_provider_configuration_id()
        {
            return None;
        }
        matched = Some((
            response.metadata().clone(),
            response.response_id().map(str::to_owned),
        ));
    }
    matched
}

/// The only serialized program counter accepted by the v14 driver.
///
/// This is semantic data at the clean pre-dispatch decision barrier, not a
/// serialized Rust future or task stack.  Keeping the variant closed makes a
/// new suspension point an explicit runtime/codec decision.
#[derive(Clone, serde::Deserialize, serde::Serialize)]
pub(in crate::runtime) enum RuntimeContinuation {
    AwaitEffectDecisionV1(Box<AwaitEffectDecisionV1>),
    AwaitToolBatchV2(Box<AwaitToolBatchV2>),
    AwaitServerPauseV1(Box<AwaitServerPauseV1>),
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct AwaitToolBatchV2 {
    pub(in crate::runtime) root: ContinuationRoot,
    pub(in crate::runtime) config: DurableAgentConfig,
    pub(in crate::runtime) budgets: TurnBudgets,
    pub(in crate::runtime) tool_calls: u64,
    pub(in crate::runtime) usage: TurnUsage,
    pub(in crate::runtime) context_anchor: Option<plakous_model::ContextAnchor>,
    pub(in crate::runtime) request_anchor: Option<Digest>,
    pub(in crate::runtime) attempt_id: AttemptId,
    pub(in crate::runtime) provider_operation: OperationId,
    pub(in crate::runtime) provider_request: ModelRequest,
    pub(in crate::runtime) provider_plan_digest: Digest,
    pub(in crate::runtime) provider_response: ModelResponse,
    pub(in crate::runtime) working: WorkingProjection,
    pub(in crate::runtime) steering: Vec<ContinuationSteering>,
    pub(in crate::runtime) slots: Vec<ContinuationToolSlot>,
    pub(in crate::runtime) observation_budget:
        crate::runtime::observation::budget::ObservationBudget,
    pub(in crate::runtime) model_admission_reference: ContinuationBindingReference,
    pub(in crate::runtime) policy_reference: ContinuationBindingReference,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct AwaitServerPauseV1 {
    pub(in crate::runtime) root: ContinuationRoot,
    pub(in crate::runtime) config: DurableAgentConfig,
    pub(in crate::runtime) budgets: TurnBudgets,
    pub(in crate::runtime) tool_calls: u64,
    pub(in crate::runtime) usage: TurnUsage,
    pub(in crate::runtime) context_anchor: Option<plakous_model::ContextAnchor>,
    pub(in crate::runtime) request_anchor: Option<Digest>,
    pub(in crate::runtime) attempt_id: AttemptId,
    pub(in crate::runtime) provider_operation: OperationId,
    pub(in crate::runtime) provider_request: ModelRequest,
    pub(in crate::runtime) provider_plan_digest: Digest,
    pub(in crate::runtime) provider_response: ModelResponse,
    pub(in crate::runtime) working: WorkingProjection,
    pub(in crate::runtime) steering: Vec<ContinuationSteering>,
    /// Observation bytes still available when the round paused.
    ///
    /// `AwaitToolBatchV2` has always carried this; the server-pause frame did
    /// not, so restoring one reset the budget to `None` and the tool results of
    /// the round after a pause were never measured against it. Nothing was
    /// clipped and nothing was reported.
    ///
    /// Capacity pressure is restored with the remaining allowance.
    pub(in crate::runtime) observation_budget:
        crate::runtime::observation::budget::ObservationBudget,
    pub(in crate::runtime) model_admission_reference: ContinuationBindingReference,
    pub(in crate::runtime) policy_reference: ContinuationBindingReference,
}

#[derive(Clone)]
pub(in crate::runtime) enum RestoredContinuationFrame {
    EffectDecision(Box<AwaitEffectDecisionV1>),
    ToolBatch(Box<RestoredHostedFrame<AwaitToolBatchV2>>),
    ServerPause(Box<RestoredHostedFrame<AwaitServerPauseV1>>),
}

#[derive(Clone)]
pub(in crate::runtime) struct RestoredHostedFrame<T> {
    pub(in crate::runtime) frame: T,
    pub(in crate::runtime) continuation_operation: OperationId,
    pub(in crate::runtime) remaining_observation_budget:
        crate::runtime::observation::budget::ObservationBudget,
    /// What the budget cut while canonicalizing this round's results.
    ///
    /// A durable session canonicalizes its tool results here, not at the
    /// driver's own call sites, so this is where an ordinary durable round's
    /// clips are found. The caller publishes them.
    pub(in crate::runtime) observation_clips: Vec<RestoredObservationClip>,
    pub(in crate::runtime) promoted: bool,
    pub(in crate::runtime) recovered_dispatched: usize,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct AwaitEffectDecisionV1 {
    pub(in crate::runtime) root: ContinuationRoot,
    pub(in crate::runtime) config: DurableAgentConfig,
    pub(in crate::runtime) budgets: TurnBudgets,
    pub(in crate::runtime) tool_calls: u64,
    pub(in crate::runtime) usage: TurnUsage,
    pub(in crate::runtime) context_anchor: Option<plakous_model::ContextAnchor>,
    pub(in crate::runtime) request_anchor: Option<Digest>,
    pub(in crate::runtime) attempt_id: AttemptId,
    pub(in crate::runtime) provider_operation: OperationId,
    pub(in crate::runtime) provider_request: ModelRequest,
    pub(in crate::runtime) provider_plan_digest: Digest,
    pub(in crate::runtime) provider_response: ModelResponse,
    pub(in crate::runtime) working: WorkingProjection,
    pub(in crate::runtime) steering: Vec<ContinuationSteering>,
    pub(in crate::runtime) slots: Vec<ContinuationToolSlot>,
    pub(in crate::runtime) pending_index: u32,
    pub(in crate::runtime) decision_operation: OperationId,
    pub(in crate::runtime) decision: EffectDecisionRef,
    pub(in crate::runtime) decision_prompt: String,
    pub(in crate::runtime) decision_expires_at: Timestamp,
    pub(in crate::runtime) dispatch_capability_minted: bool,
    pub(in crate::runtime) hosted_continuation_operation: Option<OperationId>,
    pub(in crate::runtime) observation_budget:
        crate::runtime::observation::budget::ObservationBudget,
    pub(in crate::runtime) model_admission_reference: ContinuationBindingReference,
    pub(in crate::runtime) policy_reference: ContinuationBindingReference,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct ContinuationRoot {
    pub(in crate::runtime) command_id: String,
    pub(in crate::runtime) turn_id: TurnId,
    pub(in crate::runtime) text: String,
    pub(in crate::runtime) follow_up: bool,
    pub(in crate::runtime) operation_id: OperationId,
    pub(in crate::runtime) binding: Digest,
    /// The round cap the turn was admitted under; see `PromptCommand`.
    pub(in crate::runtime) max_model_rounds: usize,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct ContinuationSteering {
    pub(in crate::runtime) text: String,
    pub(in crate::runtime) operation_id: OperationId,
}

#[derive(Clone, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct ContinuationBindingReference {
    authority: String,
    subject: String,
    revision: u64,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
pub(in crate::runtime) enum ContinuationToolSlot {
    Completed {
        call: ToolCallBlock,
        result_entry_id: EntryId,
        item: ConversationItem,
        operation_id: OperationId,
    },
    PendingDecision {
        call: ToolCallBlock,
        result_entry_id: EntryId,
        binding_digest: Digest,
    },
    NotAdmitted {
        call: ToolCallBlock,
        result_entry_id: EntryId,
    },
}

#[derive(Clone, Copy, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(in crate::runtime) struct DurableAgentConfig {
    prompt_queue_capacity: usize,
    control_queue_capacity: usize,
    steering_queue_capacity: usize,
    follow_up_queue_capacity: usize,
    event_capacity: usize,
    event_byte_capacity: usize,
    max_event_subscribers: usize,
    tool_progress_capacity: usize,
    effect_decision_timeout_nanos: u64,
    model_cleanup_timeout_nanos: u64,
    model_operation_timeout_nanos: u64,
}

pub(in crate::runtime) struct ToolBatchContinuation<'a> {
    pub(in crate::runtime) tool_calls: usize,
    pub(in crate::runtime) provider_operation: OperationId,
    pub(in crate::runtime) provider_plan: &'a ValidatedRequestPlan,
    pub(in crate::runtime) provider_response: &'a ModelResponse,
    pub(in crate::runtime) calls: &'a [ToolCallBlock],
    pub(in crate::runtime) result_entry_ids: &'a [EntryId],
    pub(in crate::runtime) items: &'a [Option<ConversationItem>],
    pub(in crate::runtime) operations: &'a [OperationId],
    pub(in crate::runtime) source_index: usize,
}
