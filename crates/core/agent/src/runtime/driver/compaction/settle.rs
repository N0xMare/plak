//! Settling a dispatched compaction: its response or its failure.

use super::{COMMITTED_PRESSURE_FOLD_CAP, CompactionAttempt, CompactionDetail, MeasuredSource};
use crate::runtime::compaction::live::LiveCompaction;
use crate::runtime::compaction::record::compaction_operation_component;
use crate::runtime::compaction::{
    CompactionCompletionError, DurableCompactionIdentity, ExactCompactionRuntime,
};
use crate::runtime::driver::Driver;
use crate::runtime::durable::DurableSessionAddress;
use crate::runtime::observation::committed_compaction_summary;
use crate::{
    AgentCompactSessionOutcome, AgentError, AgentErrorKind, AgentSessionEndpointError,
    DurableOperationState,
};
use plakous_model::ModelResponse;
use std::sync::Arc;

impl Driver {
    /// Settles a paid call that did not return a response.
    pub(super) async fn settle_compaction_failure(
        &mut self,
        attempt: &CompactionAttempt<'_>,
        measured: &MeasuredSource,
        live: &mut LiveCompaction,
        error: &AgentError,
        native_protected_history: bool,
        committed_detail: &mut CompactionDetail,
    ) -> Result<AgentCompactSessionOutcome, AgentSessionEndpointError> {
        let identity = attempt.identity;
        let origin = attempt.origin;
        let (compaction_operation, provider_operation) = live.operations();
        // A fold that did not commit may still have been billed. The
        // next rounds use the free rung with escalating backoff and
        // a per-turn failure cap. A successful fold re-arms pressure
        // for later newly consumed work.
        let failed = self
            .terminalize_compaction_failure(
                identity,
                compaction_operation,
                provider_operation,
                live.source(),
                live.record(),
                error,
                live.origin(),
                live.turn_id(),
                None,
                Some((live.prepared_record().identity(), identity.attempt_id)),
            )
            .await;
        if origin.is_turn_fence() {
            let head = self.authority.checkpoint();
            let operations = head.manifest().ledger().operations();
            let billed = operations
                .iter()
                .find(|record| record.operation_id() == provider_operation.0)
                .is_some_and(|record| {
                    record.transitions().iter().any(|transition| {
                        matches!(transition.state(), DurableOperationState::Dispatched)
                    }) && !record.transitions().iter().any(|transition| {
                        matches!(
                            transition.state(),
                            DurableOperationState::ProvenNotDispatched(_)
                        )
                    })
                });
            self.turn_budgets.paid_fold_spent |= billed;
            if !billed {
                self.turn_budgets.pressure_fold_spent = false;
            }
        }
        // A definite paid-fold failure mid-turn changed nothing, so the
        // free rung still has the whole source to work on, and the
        // turn goes on under the window instead of over it. It is a
        // new workflow: the failed fold keeps its own record. A
        // cancelled fold is not retried in any form. Endpoint folds also
        // take this unpaid rung; no failure authorizes another paid dispatch.
        if !matches!(failed, AgentCompactSessionOutcome::Failed { .. })
            || error.kind() == AgentErrorKind::Cancelled
        {
            return Ok(failed);
        }
        self.placeholders_after_failed_fold(
            attempt,
            live.runtime(),
            measured,
            native_protected_history,
            failed,
            committed_detail,
        )
        .await
    }

    /// Settles a paid call that returned a response: complete both
    /// operations, then publish the successor.
    pub(super) async fn settle_compaction_response(
        &mut self,
        attempt: &CompactionAttempt<'_>,
        measured: &MeasuredSource,
        live: &mut LiveCompaction,
        response: &ModelResponse,
        native_protected_history: bool,
        committed_detail: &mut CompactionDetail,
    ) -> Result<AgentCompactSessionOutcome, AgentSessionEndpointError> {
        let identity = attempt.identity;
        let origin = attempt.origin;
        let (compaction_operation, provider_operation) = live.operations();
        let Ok(completed) = compaction_operation_component(response) else {
            // Paid compact already completed. Encoding or byte-budget
            // failure retains its unresolved fence; it never permits replay.
            return Ok(AgentCompactSessionOutcome::RecoveryRequired {
                checkpoint: self.authority.checkpoint().id(),
                diagnostic: None,
            });
        };
        if self
            .transition_compaction_operations(
                identity,
                compaction_operation,
                provider_operation,
                identity.completed_checkpoint,
                DurableOperationState::Completed(completed),
            )
            .await
            .is_err()
        {
            return Ok(AgentCompactSessionOutcome::RecoveryRequired {
                checkpoint: self.authority.checkpoint().id(),
                diagnostic: None,
            });
        }
        match self
            .publish_compaction_success(
                identity,
                compaction_operation,
                provider_operation,
                live.record(),
                live.prepared_record(),
                response,
                Arc::clone(&live.runtime().model),
                &live.runtime().provider_configuration,
                live.runtime().clock.as_ref(),
                live.runtime().model_operation_timeout,
                live.origin(),
                live.turn_id(),
            )
            .await
        {
            Ok((target, occupancy_after)) => {
                if origin.pressure_forced() {
                    self.pressure_folds_committed += 1;
                }
                if origin.is_turn_fence() {
                    self.turn_budgets.pressure_fold_spent =
                        self.pressure_folds_committed >= COMMITTED_PRESSURE_FOLD_CAP;
                }
                Ok(AgentCompactSessionOutcome::Compacted {
                    workflow_id: identity.workflow_id,
                    source: live.source().checkpoint_id,
                    target,
                    summary: committed_compaction_summary(&self.authority).map(|summary| {
                        summary.with_occupancy(measured.occupancy_before, occupancy_after)
                    }),
                    feed_seq: self.authority.feed_seq_at(target),
                })
            }
            Err(
                CompactionCompletionError::RecoveryRequired(error)
                | CompactionCompletionError::Semantic(error),
            ) => {
                if origin.is_turn_fence() {
                    self.turn_budgets.paid_fold_spent = true;
                }
                let failed = self
                    .terminalize_compaction_failure(
                        identity,
                        compaction_operation,
                        provider_operation,
                        live.source(),
                        live.record(),
                        &error,
                        live.origin(),
                        live.turn_id(),
                        response.usage(),
                        None,
                    )
                    .await;
                if !matches!(failed, AgentCompactSessionOutcome::Failed { .. })
                    || error.kind() == AgentErrorKind::Cancelled
                {
                    return Ok(failed);
                }
                self.placeholders_after_failed_fold(
                    attempt,
                    live.runtime(),
                    measured,
                    native_protected_history,
                    failed,
                    committed_detail,
                )
                .await
            }
        }
    }

    /// The free rung after a definite paid-fold failure, as a
    /// new workflow on the current head; `failed` stands when it cannot act.
    async fn placeholders_after_failed_fold(
        &mut self,
        attempt: &CompactionAttempt<'_>,
        runtime: &ExactCompactionRuntime,
        measured: &MeasuredSource,
        native_protected_history: bool,
        failed: AgentCompactSessionOutcome,
        committed_detail: &mut CompactionDetail,
    ) -> Result<AgentCompactSessionOutcome, AgentSessionEndpointError> {
        let head = self.authority.checkpoint();
        let topology = head.manifest().topology();
        let current = DurableSessionAddress::new(
            topology.session_id,
            topology.branch_id,
            topology.checkpoint_id,
        );
        let fallback_operation = format!("{}:placeholders", attempt.request.operation_id);
        let Ok(fallback) = DurableCompactionIdentity::new(&fallback_operation, current) else {
            return Ok(failed);
        };
        match Box::pin(self.replace_protected_tool_results(
            attempt.active,
            attempt.origin,
            runtime,
            fallback,
            &fallback_operation,
            current,
            measured.projection.identity(),
            measured.harness_plan,
            measured.before,
            measured.occupancy_before,
            native_protected_history,
            &mut committed_detail.no_eligible_detail,
        ))
        .await
        {
            Ok(placeholders @ AgentCompactSessionOutcome::Compacted { .. }) => {
                if let AgentCompactSessionOutcome::Failed { diagnostic, .. } = &failed {
                    committed_detail.diagnostic.clone_from(diagnostic);
                }
                Ok(placeholders)
            }
            Ok(AgentCompactSessionOutcome::NoEligibleRange) => {
                committed_detail.exhausted = true;
                Ok(failed)
            }
            _ => Ok(failed),
        }
    }
}
