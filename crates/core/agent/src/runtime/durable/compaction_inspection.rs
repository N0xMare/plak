//! Pure exact-identity inspection shared by abandonment and its offline preflight.

use super::DurableSessionAddress;
use super::compaction_recovery::decode_paid_compaction_operations;
use crate::recovery_scope::require_recoverable;
use crate::runtime::compaction::DurableCompactionIdentity;
use crate::runtime::compaction::fold::reconstruct_compaction_cut;
use crate::{
    DurableOperationState, RuntimeCheckpoint, RuntimeLifecycle, RuntimeOperationLedger,
    RuntimeWorkingRoot,
};

/// Whether this exact paid workflow can close without dispatching its summary.
#[must_use]
pub(in crate::runtime) fn checkpoint_can_abandon_compaction(
    checkpoint: &RuntimeCheckpoint,
    operation: &str,
    source: DurableSessionAddress,
) -> bool {
    let Ok(identity) = DurableCompactionIdentity::new(operation, source) else {
        return false;
    };
    let manifest = checkpoint.manifest();
    let topology = manifest.topology();
    if topology.session_id != source.session_id
        || topology.branch_id != source.branch_id
        || !matches!(manifest.lifecycle(), RuntimeLifecycle::Working {
            root: RuntimeWorkingRoot::SessionWorkflow(root), ..
        } if *root == identity.workflow_id)
        || require_recoverable(checkpoint).is_err()
    {
        return false;
    }
    let Ok(operations) = decode_paid_compaction_operations(checkpoint, identity) else {
        return false;
    };
    if operations.request.version < 3
        && reconstruct_compaction_cut(manifest, identity, &operations.request).is_err()
    {
        return false;
    }
    ledger_can_abandon_compaction(
        manifest.ledger(),
        operations.compaction.0,
        operations.provider.0,
    )
}

fn ledger_can_abandon_compaction(
    ledger: &RuntimeOperationLedger,
    compaction: u64,
    provider: u64,
) -> bool {
    ledger.operations().into_iter().all(|record| {
        if record.operation_id() != compaction && record.operation_id() != provider {
            return matches!(record.state(), DurableOperationState::Finalized(_));
        }
        matches!(
            record.state(),
            DurableOperationState::Prepared
                | DurableOperationState::Dispatched
                | DurableOperationState::LocatorBound(_)
                | DurableOperationState::OutcomeUnknown(_)
                | DurableOperationState::Completed(_)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::continuation::runtime_component;
    use crate::{
        CheckpointId, DurableOperationKind, DurableOperationRecord, DurableOperationTransition,
        RuntimeComponentKind,
    };
    use plakous_model::{AttemptId, ModelOperationRoot, SessionWorkflowRootId};
    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn record(operation: u64, not_dispatched: bool) -> TestResult<DurableOperationRecord> {
        let request = runtime_component(
            RuntimeComponentKind::Operation,
            b"synthetic ledger evidence".to_vec(),
        )?;
        let mut transitions = vec![DurableOperationTransition::new(
            CheckpointId::try_from(uuid::Uuid::from_u128(1))?,
            DurableOperationState::Prepared,
        )];
        if not_dispatched {
            transitions.push(DurableOperationTransition::new(
                CheckpointId::try_from(uuid::Uuid::from_u128(2))?,
                DurableOperationState::ProvenNotDispatched(request.clone()),
            ));
        }
        let workflow_id = SessionWorkflowRootId::new([0xa4; 32])?;
        let kind = match operation {
            1 => DurableOperationKind::Compaction { workflow_id },
            2 => DurableOperationKind::Provider {
                root: ModelOperationRoot::SessionWorkflow(workflow_id),
                attempt_id: AttemptId::try_from(uuid::Uuid::from_u128(0xa4))?,
            },
            _ => DurableOperationKind::Command { turn_id: None },
        };
        Ok(DurableOperationRecord::new(
            operation,
            kind,
            request,
            transitions,
        )?)
    }

    #[test]
    fn foreign_non_finalized_record_prevents_compaction_abandonment() -> TestResult {
        let pair =
            RuntimeOperationLedger::new(Vec::new(), vec![record(1, false)?, record(2, false)?])?;
        assert!(ledger_can_abandon_compaction(&pair, 1, 2));
        let foreign = RuntimeOperationLedger::new(
            Vec::new(),
            vec![record(1, false)?, record(2, false)?, record(3, false)?],
        )?;
        assert!(!ledger_can_abandon_compaction(&foreign, 1, 2));
        Ok(())
    }

    /// The one-commit abandonment closure leaves no working proven-not-
    /// dispatched head, so such a record is never abandoned again.
    #[test]
    fn a_not_dispatched_record_is_never_abandoned_again() -> TestResult {
        let pair =
            RuntimeOperationLedger::new(Vec::new(), vec![record(1, true)?, record(2, true)?])?;
        assert!(!ledger_can_abandon_compaction(&pair, 1, 2));
        Ok(())
    }
}
