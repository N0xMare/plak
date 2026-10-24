//! Terminalizing a compaction that did not commit.

use crate::runtime::OperationId;
use crate::runtime::compaction::fold::compact_diagnostic;
use crate::runtime::compaction::{
    CompactOrigin, DurableCompactionIdentity, DurableCompactionRequestRecord,
};
use crate::runtime::continuation::runtime_manifest_error;
use crate::runtime::driver::Driver;
use crate::runtime::driver::model_cleanup::agent_model_timeout;
use crate::runtime::durable::DurableSessionAddress;
use crate::runtime::operation_record::{
    operation_result_component, working_lifecycle, working_lifecycle_for_root,
};
use crate::{
    AgentCompactSessionOutcome, AgentError, AgentErrorKind, DurableOperationState,
    DurableOperationTransition, RuntimeLifecycle, StoreAdmission,
};
use plakous_model::{Digest, ModelOperationRoot, TurnId, Usage};

impl Driver {
    #[allow(
        clippy::too_many_arguments,
        reason = "failure publication keeps origin and turn root beside the exact compact tuple"
    )]
    pub(super) async fn terminalize_compaction_failure(
        &mut self,
        identity: DurableCompactionIdentity,
        compaction_operation: OperationId,
        provider_operation: OperationId,
        source: DurableSessionAddress,
        record: &DurableCompactionRequestRecord,
        error: &AgentError,
        origin: CompactOrigin,
        turn_id: Option<TurnId>,
        completed_usage: Option<Usage>,
        expected_dispatch: Option<(Digest, plakous_model::AttemptId)>,
    ) -> AgentCompactSessionOutcome {
        let Ok(evidence) = operation_result_component::<()>(&Err(error.clone())) else {
            return AgentCompactSessionOutcome::RecoveryRequired {
                checkpoint: self.authority.checkpoint().id(),
                diagnostic: None,
            };
        };
        if error.kind() == AgentErrorKind::OutcomeUnknown {
            let result = self
                .transition_compaction_operations(
                    identity,
                    compaction_operation,
                    provider_operation,
                    identity.failure_evidence_checkpoint,
                    DurableOperationState::OutcomeUnknown(evidence),
                )
                .await;
            return AgentCompactSessionOutcome::RecoveryRequired {
                checkpoint: result.map_or_else(
                    |_| self.authority.checkpoint().id(),
                    |()| identity.failure_evidence_checkpoint,
                ),
                diagnostic: Some(compact_diagnostic(error)),
            };
        }
        let states = {
            let authority = self.authority.checkpoint();
            [compaction_operation, provider_operation].map(|operation| {
                authority
                    .manifest()
                    .ledger()
                    .operation(operation.0)
                    .map(|record| record.state().clone())
            })
        };
        if states.iter().any(Option::is_none) {
            return AgentCompactSessionOutcome::RecoveryRequired {
                checkpoint: self.authority.checkpoint().id(),
                diagnostic: None,
            };
        }
        let proven_pre_dispatch = matches!(
            &states,
            [
                Some(DurableOperationState::Prepared),
                Some(DurableOperationState::Prepared | DurableOperationState::LocatorBound(_)),
            ]
        );
        if proven_pre_dispatch {
            let (history, ledger, context, family) = {
                let authority = self.authority.checkpoint();
                let manifest = authority.manifest();
                let ledger = manifest
                    .ledger()
                    .transition_operation(
                        compaction_operation.0,
                        DurableOperationTransition::new(
                            identity.failure_evidence_checkpoint,
                            DurableOperationState::ProvenNotDispatched(evidence.clone()),
                        ),
                    )
                    .and_then(|ledger| {
                        ledger.transition_operation(
                            provider_operation.0,
                            DurableOperationTransition::new(
                                identity.failure_evidence_checkpoint,
                                DurableOperationState::ProvenNotDispatched(evidence.clone()),
                            ),
                        )
                    });
                let Ok(ledger) = ledger else {
                    return AgentCompactSessionOutcome::RecoveryRequired {
                        checkpoint: authority.id(),
                        diagnostic: None,
                    };
                };
                (
                    manifest.canonical().history().clone(),
                    ledger,
                    manifest.canonical().context().retained_for_exact_resume(),
                    manifest.family_artifact().cloned(),
                )
            };
            let Ok(lifecycle) = working_lifecycle_for_root(
                ModelOperationRoot::SessionWorkflow(identity.workflow_id),
                &ledger,
            ) else {
                return AgentCompactSessionOutcome::RecoveryRequired {
                    checkpoint: self.authority.checkpoint().id(),
                    diagnostic: None,
                };
            };
            if self
                .publish_checkpoint(
                    StoreAdmission::Settlement,
                    identity.failure_evidence_checkpoint,
                    lifecycle,
                    history,
                    ledger,
                    context,
                    family,
                )
                .await
                .is_err()
            {
                return AgentCompactSessionOutcome::RecoveryRequired {
                    checkpoint: self.authority.checkpoint().id(),
                    diagnostic: None,
                };
            }
        } else if states.iter().any(|state| {
            !matches!(
                state,
                Some(
                    DurableOperationState::Completed(_)
                        | DurableOperationState::ProvenNotDispatched(_),
                )
            )
        }) && self
            .transition_compaction_operations(
                identity,
                compaction_operation,
                provider_operation,
                identity.failure_evidence_checkpoint,
                // A start that local admission or final revalidation refused
                // carries exact proof that nothing was sent: unpaid, not
                // merely definite.
                if expected_dispatch.is_some_and(|(prepared_call, attempt_id)| {
                    error
                        .model_dispatch_rejection()
                        .is_some_and(|proof| proof.matches(prepared_call, attempt_id))
                }) {
                    DurableOperationState::ProvenNotDispatched(evidence.clone())
                } else {
                    DurableOperationState::Completed(evidence.clone())
                },
            )
            .await
            .is_err()
        {
            return AgentCompactSessionOutcome::RecoveryRequired {
                checkpoint: self.authority.checkpoint().id(),
                diagnostic: None,
            };
        }
        let publication = (|| -> Result<_, AgentError> {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            let topology = manifest.topology();
            if (topology.session_id, topology.branch_id) != (source.session_id, source.branch_id)
                || manifest.canonical().history().identity() != record.source_history
            {
                return Err(AgentError::new(
                    AgentErrorKind::OutcomeUnknown,
                    "compaction failure publication lost its exact semantic source",
                ));
            }
            let mut ledger = manifest.ledger().clone();
            for operation in [compaction_operation, provider_operation] {
                let terminal = if operation == compaction_operation {
                    evidence.clone()
                } else {
                    ledger
                        .operation(operation.0)
                        .and_then(|record| match record.state() {
                            DurableOperationState::Completed(value)
                            | DurableOperationState::ProvenNotDispatched(value) => {
                                Some(value.clone())
                            }
                            _ => None,
                        })
                        .ok_or_else(|| {
                            AgentError::new(
                                AgentErrorKind::OutcomeUnknown,
                                "compaction provider failure lacks definite terminal evidence",
                            )
                        })?
                };
                ledger = ledger
                    .transition_operation(
                        operation.0,
                        DurableOperationTransition::new(
                            identity.failure_checkpoint,
                            DurableOperationState::Finalized(terminal),
                        ),
                    )
                    .map_err(runtime_manifest_error)?;
            }
            Ok((
                manifest.canonical().history().clone(),
                ledger,
                manifest.canonical().context().retained_for_exact_resume(),
            ))
        })();
        let Ok((history, ledger, context)) = publication else {
            return AgentCompactSessionOutcome::RecoveryRequired {
                checkpoint: self.authority.checkpoint().id(),
                diagnostic: None,
            };
        };
        let family = self
            .authority
            .checkpoint()
            .manifest()
            .family_artifact()
            .cloned();
        let failure_lifecycle = match (origin, turn_id) {
            (CompactOrigin::TurnFence { .. }, Some(turn_id)) => {
                match working_lifecycle(turn_id, &ledger) {
                    Ok(lifecycle) => lifecycle,
                    Err(_) => {
                        return AgentCompactSessionOutcome::RecoveryRequired {
                            checkpoint: self.authority.checkpoint().id(),
                            diagnostic: None,
                        };
                    }
                }
            }
            _ => RuntimeLifecycle::Quiescent,
        };
        match self
            .publish_checkpoint(
                StoreAdmission::Settlement,
                identity.failure_checkpoint,
                failure_lifecycle,
                history,
                ledger,
                context,
                family,
            )
            .await
        {
            Ok(_) => AgentCompactSessionOutcome::Failed {
                checkpoint: identity.failure_checkpoint,
                diagnostic: Some(compact_diagnostic(error)),
                usage: completed_usage,
            },
            Err(_) => AgentCompactSessionOutcome::RecoveryRequired {
                checkpoint: self.authority.checkpoint().id(),
                diagnostic: None,
            },
        }
    }

    pub(super) fn abandoned_compaction_timeout(&self) -> AgentError {
        agent_model_timeout(
            &self.model_error_target,
            "compaction was abandoned when its deadline elapsed after dispatch; it was not \
             applied and may have been billed",
        )
    }
}
