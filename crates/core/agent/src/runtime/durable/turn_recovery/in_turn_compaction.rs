//! Completed paid-fold evidence is not proof that the fold was applied.

use super::{
    AgentError, CheckpointId, DurableOperationKind, DurableOperationRecord, DurableOperationState,
    DurableOperationTransition, ModelOperationRoot, OperationId, RecoveryAction,
    RecoveryObligation, RecoveryObligationKind, RuntimeOperationLedger, SessionStoreError,
    runtime_manifest_error,
};

pub(super) fn abandonable_debt(
    record: &DurableOperationRecord,
    obligation: &RecoveryObligation,
) -> bool {
    matches!(
        (
            record.kind(),
            obligation.kind(),
            obligation.action(),
            record.state()
        ),
        (
            DurableOperationKind::Compaction { .. },
            RecoveryObligationKind::Compaction,
            RecoveryAction::FinalizationRequired,
            DurableOperationState::Completed(_)
        ) | (
            DurableOperationKind::Provider {
                root: ModelOperationRoot::SessionWorkflow(_),
                ..
            },
            RecoveryObligationKind::ProviderAttempt,
            RecoveryAction::FinalizationRequired,
            DurableOperationState::Completed(_)
        )
    ) || matches!(
        (
            record.kind(),
            obligation.kind(),
            obligation.action(),
            record.state()
        ),
        (
            DurableOperationKind::Compaction { .. },
            RecoveryObligationKind::Compaction,
            RecoveryAction::ProviderEvidenceRequired,
            DurableOperationState::Prepared
                | DurableOperationState::Dispatched
                | DurableOperationState::LocatorBound(_)
                | DurableOperationState::OutcomeUnknown(_)
        ) | (
            DurableOperationKind::Provider {
                root: ModelOperationRoot::SessionWorkflow(_),
                ..
            },
            RecoveryObligationKind::ProviderAttempt,
            RecoveryAction::ProviderEvidenceRequired,
            DurableOperationState::Prepared
                | DurableOperationState::Dispatched
                | DurableOperationState::LocatorBound(_)
                | DurableOperationState::OutcomeUnknown(_)
        )
    )
}

pub(super) fn finalize_completed_fold(
    ledger: &RuntimeOperationLedger,
    operation: OperationId,
    checkpoint_id: CheckpointId,
) -> Result<Option<RuntimeOperationLedger>, AgentError> {
    let record = ledger
        .operation(operation.0)
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
    let DurableOperationState::Completed(evidence) = record.state() else {
        return Ok(None);
    };
    if !matches!(
        record.kind(),
        DurableOperationKind::Compaction { .. }
            | DurableOperationKind::Provider {
                root: ModelOperationRoot::SessionWorkflow(_),
                ..
            }
    ) {
        return Ok(None);
    }
    ledger
        .transition_operation(
            operation.0,
            DurableOperationTransition::new(
                checkpoint_id,
                DurableOperationState::Finalized(evidence.clone()),
            ),
        )
        .map(Some)
        .map_err(runtime_manifest_error)
}

#[cfg(test)]
mod tests;
