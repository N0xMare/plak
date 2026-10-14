use crate::AgentError;
use crate::local_composition::{
    LocalCompositionEventReceiver, LocalCompositionSession, LocalCompositionSessionLoss,
    LocalCompositionShutdownResult, LocalCompositionTerminateResult,
};
use crate::manifest::{
    InterpreterAcknowledgedBoundary, InterpreterAvailability, InterpreterObservation,
    InterpreterResetReason,
};
use crate::runtime::OperationId;
use crate::runtime::cleanup::{
    combine_cleanup_results, contain_cleanup_panic, emit_shutdown_child_exit,
    error_with_cleanup_disposition, local_composition_shutdown_result,
};
use crate::runtime::composition::projection::local_composition_started_error;
use crate::runtime::composition::{
    CompositionOwner, LocalCompositionRetentionState, LocalCompositionShutdownTelemetry,
    retain_local_composition_at_root_boundary,
};
use crate::runtime::continuation::runtime_manifest_error;
use plakous_model::Digest;
use plakous_tools::ToolInvocation;
use std::time::Duration;
/// One transfer obligation: the interpreter and its receiver are retained,
/// restored, or explicitly joined together by the current actor future.
pub(in super::super) struct InterpreterResources {
    session: Box<dyn LocalCompositionSession>,
    events: Box<dyn LocalCompositionEventReceiver>,
}

impl InterpreterResources {
    pub(in super::super) fn new(
        session: Box<dyn LocalCompositionSession>,
        events: Box<dyn LocalCompositionEventReceiver>,
    ) -> Self {
        Self { session, events }
    }
}

pub(in crate::runtime) fn take_local_composition_owner(
    state: &mut CompositionOwner,
) -> Result<InterpreterResources, AgentError> {
    state.resources.take().ok_or_else(|| {
        local_composition_started_error("local composition session/broker owner disappeared")
    })
}

pub(in crate::runtime) async fn join_extracted_local_composition_owner(
    state: &mut CompositionOwner,
    resources: InterpreterResources,
    terminate_active_cell: bool,
    telemetry: Option<LocalCompositionShutdownTelemetry>,
) -> Vec<AgentError> {
    let mut errors = Vec::new();
    // Poll every queued batch reply before closing its receiver. A healthy
    // batch can acknowledge concurrently; a stalled interpreter never delays
    // cancellation beyond this bound. Timeout retains the exact obligations
    // for settlement after termination and shutdown.
    let deliveries = async {
        while let Some(result) = state.next_delivery().await {
            if let Err(error) = result {
                errors.push(error);
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(1), deliveries).await;
    if terminate_active_cell && let Some(cell) = state.cell.clone() {
        match contain_cleanup_panic(async { Ok(resources.session.terminate(cell).await) }).await {
            Ok(LocalCompositionTerminateResult::Joined(_)) => {
                state.retired_cell = state.cell.take();
                state.source_digest = None;
            }
            Ok(LocalCompositionTerminateResult::Lost(error)) => {
                if let Err(observation) = observe_local_composition_session_loss(state, &error) {
                    errors.push(observation);
                }
                errors.push(local_composition_started_error(error));
            }
            Err(error) => errors.push(error),
        }
    }
    match contain_cleanup_panic(async { Ok(resources.session.shutdown().await) }).await {
        Ok(LocalCompositionShutdownResult::Joined { child_exit }) => {
            if let Some(telemetry) = telemetry.as_ref() {
                emit_shutdown_child_exit(
                    Some(telemetry),
                    &LocalCompositionShutdownResult::Joined { child_exit },
                );
            }
            state.possible_start = false;
        }
        Ok(LocalCompositionShutdownResult::Lost(error)) => {
            if let Err(observation) = observe_local_composition_session_loss(state, &error) {
                errors.push(observation);
            }
            state.possible_start = true;
            errors.push(local_composition_started_error(error));
        }
        Err(error) => {
            state.possible_start = true;
            errors.push(error);
        }
    }
    drop(resources);
    errors.extend(state.settle_deliveries().await);
    errors
}

pub(in crate::runtime) fn observe_local_composition_session_loss(
    state: &mut CompositionOwner,
    loss: &LocalCompositionSessionLoss,
) -> Result<(), AgentError> {
    let Some(observation) = state.interpreter_observation.as_ref() else {
        return Ok(());
    };
    let reason = local_composition_reset_reason(loss);
    state.interpreter_observation = Some(
        InterpreterObservation::new(
            observation.generation(),
            InterpreterAvailability::Reset,
            Some(reason),
            state.cell.clone(),
            observation.last_acknowledged(),
        )
        .map_err(runtime_manifest_error)?,
    );
    state.possible_start = true;
    Ok(())
}

pub(in crate::runtime) fn local_composition_session_loss_error(
    state: &mut CompositionOwner,
    loss: &LocalCompositionSessionLoss,
) -> AgentError {
    let primary = local_composition_started_error(loss);
    match observe_local_composition_session_loss(state, loss) {
        Ok(()) => primary,
        Err(observation) => error_with_cleanup_disposition(primary, &[observation]),
    }
}

pub(in crate::runtime) fn local_composition_reset_reason(
    loss: &LocalCompositionSessionLoss,
) -> InterpreterResetReason {
    match loss {
        LocalCompositionSessionLoss::HostLost(_) => InterpreterResetReason::HostLost,
        LocalCompositionSessionLoss::ProtocolViolation(_) => {
            InterpreterResetReason::ProtocolViolation
        }
        LocalCompositionSessionLoss::Cancellation(_) => InterpreterResetReason::Cancellation,
        LocalCompositionSessionLoss::AgentShutdown(_) => InterpreterResetReason::AgentShutdown,
    }
}

impl CompositionOwner {
    pub(in super::super) async fn cleanup_at_root_boundary(
        &mut self,
        shutdown_telemetry: LocalCompositionShutdownTelemetry,
    ) -> Result<Option<OperationId>, AgentError> {
        if self.possible_start && self.resources.is_none() {
            return Err(local_composition_started_error(
                "possible source start remains without live cleanup authority",
            ));
        }
        if retain_local_composition_at_root_boundary(
            self.integration.lifecycle(),
            LocalCompositionRetentionState {
                has_session: self.resources.is_some(),
                has_events: self.resources.is_some(),
                has_cell: self.cell.is_some(),
                possible_start: self.possible_start,
                has_pending_nested: !self.pending_is_empty(),
                has_active_nested: self.has_active_nested()
                    || !self.completed_is_empty()
                    || !self.deliveries_are_empty(),
                healthy_idle_observed: self.interpreter_observation.as_ref().is_some_and(
                    |observation| {
                        observation.availability() == InterpreterAvailability::Available
                            && observation.affected_cell().is_none()
                            && observation.last_acknowledged()
                                == Some(InterpreterAcknowledgedBoundary::Terminal)
                    },
                ),
            },
        ) {
            return Ok(None);
        }
        if self.resources.is_some() {
            let resources = take_local_composition_owner(self)?;
            let errors = join_extracted_local_composition_owner(
                self,
                resources,
                true,
                Some(shutdown_telemetry),
            )
            .await;
            if let Some((first, remaining)) = errors.split_first() {
                return Err(error_with_cleanup_disposition(first.clone(), remaining));
            }
        }
        self.source_digest = None;
        self.reset_nested_after_cleanup();
        self.child_operations.clear();
        Ok(self.evaluation_operation.take())
    }
    pub(in super::super) async fn retire_session(
        &mut self,
        shutdown_telemetry: LocalCompositionShutdownTelemetry,
    ) -> Result<(), AgentError> {
        let session_result = contain_cleanup_panic(async {
            let result = match self.resources.as_ref() {
                Some(resources) => resources.session.shutdown().await,
                None => LocalCompositionShutdownResult::Joined { child_exit: None },
            };
            emit_shutdown_child_exit(Some(&shutdown_telemetry), &result);
            local_composition_shutdown_result(result)
        })
        .await;
        let integration_result = contain_cleanup_panic(async {
            let result = self.integration.shutdown().await;
            emit_shutdown_child_exit(Some(&shutdown_telemetry), &result);
            local_composition_shutdown_result(result)
        })
        .await;
        let delivery_result = contain_cleanup_panic(async {
            let delivery_errors = self.settle_deliveries().await;
            if delivery_errors.is_empty() {
                Ok(())
            } else {
                Err(error_with_cleanup_disposition(
                    local_composition_started_error(
                        "session closed with unconfirmed nested deliveries",
                    ),
                    &delivery_errors,
                ))
            }
        })
        .await;
        combine_cleanup_results([session_result, integration_result, delivery_result])
    }
    pub(in super::super) fn acknowledge_launch(&mut self) -> Result<(), AgentError> {
        if let Some(observation) = self.interpreter_observation.as_ref() {
            self.interpreter_observation = Some(
                InterpreterObservation::new(
                    observation.generation(),
                    InterpreterAvailability::Available,
                    None,
                    None,
                    Some(InterpreterAcknowledgedBoundary::Launch),
                )
                .map_err(runtime_manifest_error)?,
            );
        }
        Ok(())
    }
    pub(in super::super) fn acknowledge_source(
        &mut self,
        cell_id: String,
    ) -> Result<(), AgentError> {
        self.cell = Some(cell_id.clone());
        self.retired_cell = None;
        if let Some(observation) = self.interpreter_observation.as_ref() {
            self.interpreter_observation = Some(
                InterpreterObservation::new(
                    observation.generation(),
                    InterpreterAvailability::Available,
                    None,
                    Some(cell_id),
                    Some(InterpreterAcknowledgedBoundary::SourceAccepted),
                )
                .map_err(runtime_manifest_error)?,
            );
        }
        Ok(())
    }
    pub(in super::super) fn acknowledge_terminal(&mut self) -> Result<(), AgentError> {
        if let Some(observation) = self.interpreter_observation.as_ref() {
            self.interpreter_observation = Some(
                InterpreterObservation::new(
                    observation.generation(),
                    InterpreterAvailability::Available,
                    None,
                    None,
                    Some(InterpreterAcknowledgedBoundary::Terminal),
                )
                .map_err(runtime_manifest_error)?,
            );
        }
        Ok(())
    }
    pub(in super::super) fn acknowledge_termination(&mut self) -> Result<(), AgentError> {
        if let Some(observation) = self.interpreter_observation.as_ref() {
            self.interpreter_observation = Some(
                InterpreterObservation::new(
                    observation.generation(),
                    InterpreterAvailability::Reset,
                    Some(InterpreterResetReason::Cancellation),
                    None,
                    Some(InterpreterAcknowledgedBoundary::Join),
                )
                .map_err(runtime_manifest_error)?,
            );
        }
        Ok(())
    }
    pub(in super::super) fn begin_evaluation(
        &mut self,
        operation_id: OperationId,
        invocation: &ToolInvocation,
    ) {
        self.evaluation_operation = Some(operation_id);
        self.exec_call_id = Some(invocation.call_id());
        self.exec_remote_call_id = Some(invocation.call_id().to_string());
        self.exec_correlation = Some(invocation.correlation().clone());
        self.pending_updates.clear();
        self.possible_start = true;
        self.started_at = Some(tokio::time::Instant::now());
        self.reset_nested_for_cell();
    }

    pub(in super::super) fn take_resources(&mut self) -> Option<InterpreterResources> {
        self.resources.take()
    }
    pub(in super::super) fn restore_resources(&mut self, resources: InterpreterResources) {
        self.resources = Some(resources);
    }
    pub(in super::super) fn restore_terminal_resources(&mut self, resources: InterpreterResources) {
        self.retire_cell();
        self.restore_resources(resources);
    }
    pub(in super::super) fn retire_cell(&mut self) {
        self.retired_cell = self.cell.take();
        self.source_digest = None;
        self.possible_start = false;
    }
    pub(in super::super) fn record_source_digest(&mut self, digest: Digest) {
        self.source_digest = Some(digest);
    }
}

impl InterpreterResources {
    pub(in super::super) fn generation_lost(&self) -> bool {
        self.session.generation_lost()
    }
    pub(in super::super) fn parts(
        &mut self,
    ) -> (
        &dyn LocalCompositionSession,
        &mut dyn LocalCompositionEventReceiver,
    ) {
        (self.session.as_ref(), self.events.as_mut())
    }
}
