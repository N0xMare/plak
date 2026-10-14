//! Exact sibling settlement under an already-recorded root terminal.

use super::{
    AgentError, CheckpointId, DurableOperationKind, DurableOperationRecord, DurableOperationState,
    OperationId, ReconstructiveComponent, RecoveryAction, RecoveryObligation,
    RecoveryObligationKind, RuntimeCheckpoint, RuntimeLifecycle, RuntimeOperationLedger,
    SessionStoreError, abandoned_effect_evidence, abandoned_operation_evidence,
    abandoned_process_evidence, close_as_unobserved,
};

pub(super) fn uncertain_evidence(
    record: &DurableOperationRecord,
    obligation: &RecoveryObligation,
) -> Result<Option<ReconstructiveComponent>, AgentError> {
    let expected = match record.kind() {
        DurableOperationKind::Provider { .. } => (
            RecoveryObligationKind::ProviderAttempt,
            RecoveryAction::ProviderEvidenceRequired,
        ),
        DurableOperationKind::Effect { .. } => (
            RecoveryObligationKind::Effect,
            RecoveryAction::EffectEvidenceRequired,
        ),
        DurableOperationKind::ProcessSession { .. } => (
            RecoveryObligationKind::ProcessSession,
            RecoveryAction::EffectEvidenceRequired,
        ),
        _ => return Ok(None),
    };
    if (obligation.kind(), obligation.action()) != expected
        || !matches!(
            record.state(),
            DurableOperationState::Dispatched
                | DurableOperationState::Claimed(_)
                | DurableOperationState::LocatorBound(_)
                | DurableOperationState::OutcomeUnknown(_)
        )
    {
        return Ok(None);
    }
    if let DurableOperationState::OutcomeUnknown(evidence) = record.state() {
        return Ok(Some(evidence.clone()));
    }
    Ok(Some(match record.kind() {
        DurableOperationKind::Effect { .. } => abandoned_effect_evidence()?,
        DurableOperationKind::ProcessSession { .. } => abandoned_process_evidence()?,
        _ => abandoned_operation_evidence()?,
    }))
}

pub(super) fn settle_uncertain(
    source: &RuntimeCheckpoint,
    ledger: &RuntimeOperationLedger,
    operation: OperationId,
    checkpoint_id: CheckpointId,
) -> Result<Option<RuntimeOperationLedger>, AgentError> {
    let RuntimeLifecycle::Working { obligations, .. } = source.manifest().lifecycle() else {
        return Ok(None);
    };
    let obligation = obligations
        .iter()
        .find(|item| item.operation_id() == operation.0)
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
    let record = ledger
        .operation(operation.0)
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
    uncertain_evidence(record, obligation)?
        .map(|evidence| close_as_unobserved(ledger, operation, &evidence, checkpoint_id))
        .transpose()
}
