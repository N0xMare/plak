//! Same-turn controls are not provider or tool work.

use super::{
    AgentError, CheckpointId, DurableCommandDisposition, DurableOperationKind,
    DurableOperationRecord, DurableOperationState, DurableOperationTransition,
    RuntimeComponentKind, RuntimeOperationLedger, SessionStoreError, TurnId, runtime_component,
    runtime_manifest_error,
};
use crate::runtime::{DurableActorCommandEffect, DurableCommandEnvelope};

pub(super) const STEERING_NOT_APPLIED: &[u8] = b"steering-not-applied-turn-ended";

pub(super) fn same_turn_control(
    ledger: &RuntimeOperationLedger,
    record: &DurableOperationRecord,
    turn_id: TurnId,
) -> bool {
    matches!(record.kind(), DurableOperationKind::Command { turn_id: Some(bound) } if *bound == turn_id)
        && ledger.commands().into_iter().any(|command| {
            command.turn_id() == Some(turn_id)
                && command.disposition() == DurableCommandDisposition::Accepted
                && command.command_component().bytes() == record.request().bytes()
                && serde_json::from_slice::<DurableCommandEnvelope>(record.request().bytes())
                    .is_ok_and(|envelope| {
                        envelope.version == 1
                            && envelope.rejection.is_none()
                            && matches!(
                                envelope.effect,
                                DurableActorCommandEffect::SteerQueued { position: 1.. }
                                    | DurableActorCommandEffect::CancelRequested
                                    | DurableActorCommandEffect::CancelAlreadyRequested
                                    | DurableActorCommandEffect::CancelAlreadyTerminal
                                    | DurableActorCommandEffect::EffectDecisionAllowed
                                    | DurableActorCommandEffect::EffectDecisionDenied
                                    | DurableActorCommandEffect::Shutdown
                            )
                    })
        })
}

pub(in crate::runtime) fn has_unapplied_steering(
    ledger: &RuntimeOperationLedger,
    turn_id: TurnId,
) -> bool {
    ledger.operations().into_iter().any(|record| {
        same_turn_control(ledger, record, turn_id)
            && serde_json::from_slice::<DurableCommandEnvelope>(record.request().bytes()).is_ok_and(
                |envelope| {
                    matches!(
                        envelope.effect,
                        DurableActorCommandEffect::SteerQueued { .. }
                    )
                },
            )
            && matches!(record.state(),
                DurableOperationState::Finalized(evidence)
                | DurableOperationState::Completed(evidence)
                | DurableOperationState::ProvenNotDispatched(evidence)
                if steering_not_applied(evidence.bytes(), record))
    })
}

/// Whether a steer operation's evidence says the turn ended before the steer
/// applied: a recovering closure's tag, or the live terminal's closure of a
/// pending command.
pub(in crate::runtime) fn steering_not_applied(
    evidence: &[u8],
    record: &DurableOperationRecord,
) -> bool {
    evidence == STEERING_NOT_APPLIED
        || serde_json::from_slice::<(String, u64, Vec<u8>)>(evidence).is_ok_and(
            |(tag, operation_id, request)| {
                tag == "command-terminal-closure-v1"
                    && operation_id == record.operation_id()
                    && request == record.request().bytes()
            },
        )
}

pub(super) fn settle_control(
    ledger: &RuntimeOperationLedger,
    record: &DurableOperationRecord,
    checkpoint_id: CheckpointId,
) -> Result<RuntimeOperationLedger, AgentError> {
    let steering = serde_json::from_slice::<DurableCommandEnvelope>(record.request().bytes())
        .is_ok_and(|envelope| {
            matches!(
                envelope.effect,
                DurableActorCommandEffect::SteerQueued { .. }
            )
        });
    let evidence = runtime_component(
        RuntimeComponentKind::Operation,
        if steering {
            STEERING_NOT_APPLIED.to_vec()
        } else {
            b"control-not-applied-turn-ended".to_vec()
        },
    )?;
    let states = match record.state() {
        DurableOperationState::Prepared => vec![
            DurableOperationState::ProvenNotDispatched(evidence.clone()),
            DurableOperationState::Finalized(evidence),
        ],
        DurableOperationState::Dispatched => vec![
            DurableOperationState::Completed(evidence.clone()),
            DurableOperationState::Finalized(evidence),
        ],
        DurableOperationState::Completed(completed) => {
            // Live promotion appends the Steering segment and finalizes the
            // operation atomically, so an unfinalized Completed steer is unapplied.
            vec![DurableOperationState::Finalized(if steering {
                evidence
            } else {
                completed.clone()
            })]
        }
        DurableOperationState::ProvenNotDispatched(proof) => {
            vec![DurableOperationState::Finalized(proof.clone())]
        }
        DurableOperationState::Finalized(_) => return Ok(ledger.clone()),
        _ => return Err(AgentError::from_store(SessionStoreError::corrupt())),
    };
    ledger
        .transition_operation_many(
            record.operation_id(),
            states
                .into_iter()
                .map(|state| DurableOperationTransition::new(checkpoint_id, state)),
        )
        .map_err(runtime_manifest_error)
}

#[cfg(test)]
pub(super) mod tests;
