//! Exact reference closure of the retained live-turn frontier.

use super::{
    AgentError, BTreeSet, DurableOperationKind, DurableOperationRecord, ModelOperationRoot,
    RuntimeOperationLedger, TurnId, corrupt, groups, workflow,
};
use crate::runtime::OperationId;
use crate::runtime::continuation::{
    ContinuationRoot, ContinuationSteering, ContinuationToolSlot, RuntimeContinuation,
};
use crate::runtime::operation_record::provider_outcome;

pub(super) fn close_dependencies(
    ledger: &RuntimeOperationLedger,
    pins: &mut BTreeSet<u64>,
    turn: TurnId,
) -> Result<(), AgentError> {
    let records = ledger.operations();
    let groups = groups(ledger);
    loop {
        let before = pins.len();
        for record in &records {
            if pins.contains(&record.operation_id()) {
                for dependency in record_dependencies(record, turn)? {
                    if ledger.operation(dependency).is_none() {
                        return Err(corrupt());
                    }
                    pins.insert(dependency);
                }
            }
        }
        let workflows = records
            .iter()
            .filter(|record| pins.contains(&record.operation_id()))
            .filter_map(|record| workflow(record))
            .collect::<BTreeSet<_>>();
        pins.extend(
            records
                .iter()
                .filter(|record| workflow(record).is_some_and(|id| workflows.contains(&id)))
                .map(|record| record.operation_id()),
        );
        let live_groups = groups
            .iter()
            .filter(|(id, _)| pins.contains(id))
            .map(|(_, group)| *group)
            .collect::<BTreeSet<_>>();
        pins.extend(
            groups
                .iter()
                .filter(|(_, group)| live_groups.contains(group))
                .map(|(id, _)| *id),
        );
        if pins.len() == before {
            return Ok(());
        }
    }
}

pub(super) fn record_dependencies(
    record: &DurableOperationRecord,
    turn: TurnId,
) -> Result<BTreeSet<u64>, AgentError> {
    let mut pins = BTreeSet::new();
    // Earlier roots' steering can already be pruned at quiescence. Only the
    // live root's steering participates in frame and recovery reconstruction.
    if matches!(record.kind(), DurableOperationKind::Provider {
        root: ModelOperationRoot::Turn(root), .. } if *root == turn)
        && let Some(outcome) = provider_outcome(record)?
    {
        pins.extend(outcome.steering.iter().map(|operation| operation.0));
    }
    if !matches!(
        record.kind(),
        DurableOperationKind::ToolBatchContinuation { .. }
            | DurableOperationKind::ServerPauseContinuation { .. }
            | DurableOperationKind::EffectDecision { .. }
    ) {
        return Ok(pins);
    }
    let frame: RuntimeContinuation =
        serde_json::from_slice(record.request().bytes()).map_err(|_| corrupt())?;
    match frame {
        RuntimeContinuation::AwaitToolBatchV2(frame) => frame_dependencies(
            &frame.root,
            frame.provider_operation,
            &frame.steering,
            &frame.slots,
            &mut pins,
        ),
        RuntimeContinuation::AwaitServerPauseV1(frame) => frame_dependencies(
            &frame.root,
            frame.provider_operation,
            &frame.steering,
            &[],
            &mut pins,
        ),
        RuntimeContinuation::AwaitEffectDecisionV1(frame) => {
            frame_dependencies(
                &frame.root,
                frame.provider_operation,
                &frame.steering,
                &frame.slots,
                &mut pins,
            );
            pins.insert(frame.decision_operation.0);
            pins.extend(
                frame
                    .hosted_continuation_operation
                    .map(|operation| operation.0),
            );
        }
    }
    Ok(pins)
}

fn frame_dependencies(
    root: &ContinuationRoot,
    provider: OperationId,
    steering: &[ContinuationSteering],
    slots: &[ContinuationToolSlot],
    pins: &mut BTreeSet<u64>,
) {
    pins.extend([root.operation_id.0, provider.0]);
    pins.extend(steering.iter().map(|steer| steer.operation_id.0));
    pins.extend(slots.iter().filter_map(|slot| match slot {
        ContinuationToolSlot::Completed { operation_id, .. } => Some(operation_id.0),
        _ => None,
    }));
}
