//! Observe an executing or continuing cell without relinquishing nested settlement authority.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the composition driver that shares its vocabulary"
)]

use super::*;
use crate::local_composition::{
    LocalCompositionEventReceiver, LocalCompositionFuture, LocalCompositionSessionFuture,
};

pub(super) enum LocalCompositionObservation<'a> {
    Execute {
        cell_id: String,
        response: LocalCompositionSessionFuture<'static, LocalCompositionResponse>,
    },
    Wait {
        cell_id: String,
        response: LocalCompositionFuture<'a, LocalCompositionWaitOutcome>,
        observation_timeout: Duration,
    },
}

impl LocalCompositionObservation<'_> {
    fn timeout(&self, operation_timeout: Duration, steer_pending: bool) -> Duration {
        match self {
            Self::Wait {
                observation_timeout,
                ..
            } if !steer_pending => *observation_timeout,
            Self::Execute { .. } | Self::Wait { .. } => operation_timeout,
        }
    }

    fn deadline_error(&self) -> &'static str {
        match self {
            Self::Execute { .. } => {
                "local composition initial response exceeded its bounded operation deadline"
            }
            Self::Wait { .. } => "local composition wait exceeded its bounded operation deadline",
        }
    }

    fn cell_id(&self) -> &str {
        match self {
            Self::Execute { cell_id, .. } | Self::Wait { cell_id, .. } => cell_id,
        }
    }

    async fn receive(&mut self) -> ObservedResponse {
        match self {
            Self::Execute { response, .. } => ObservedResponse::Execute(response.await),
            Self::Wait { response, .. } => ObservedResponse::Wait(response.await),
        }
    }
}

enum ObservedResponse {
    Execute(Result<LocalCompositionResponse, LocalCompositionSessionLoss>),
    Wait(Result<LocalCompositionWaitOutcome, String>),
}

impl ObservedResponse {
    fn resolve(self, state: &mut CompositionOwner) -> Result<LocalCompositionResponse, AgentError> {
        match self {
            Self::Execute(response) => {
                response.map_err(|loss| local_composition_session_loss_error(state, &loss))
            }
            Self::Wait(response) => match response {
                Err(error) => Err(local_composition_started_error(error)),
                Ok(LocalCompositionWaitOutcome::Lost(loss)) => {
                    match observe_local_composition_session_loss(state, &loss) {
                        Ok(()) => Err(local_composition_started_error(loss)),
                        Err(error) => Err(error),
                    }
                }
                Ok(outcome) => local_composition_wait_response(outcome),
            },
        }
    }
}

impl Driver {
    pub(super) async fn observe_local_composition_cell(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        state: &mut CompositionOwner,
        events: &mut dyn LocalCompositionEventReceiver,
        nested_executions: &mut FuturesUnordered<LocalCompositionNestedFuture>,
        observation: &mut LocalCompositionObservation<'_>,
    ) -> Result<LocalCompositionResponse, AgentError> {
        let operation_timeout = state
            .integration()
            .foreground_budget()
            .map_or(self.config.model_operation_timeout, |budget| {
                budget.saturating_add(Duration::from_secs(5))
            });
        let response_deadline = tokio::time::sleep(observation.timeout(operation_timeout, false));
        tokio::pin!(response_deadline);
        let mut steer_settlement_started = false;
        let mut nested_was_settled = true;
        let mut job_changes = self.job_sessions.job_terminal_changes();
        job_changes.mark_changed();
        loop {
            let steer_pending = state.integration().runtime_contract().supports_yield
                && matches!(
                    *self.tool_preempt.borrow(),
                    plakous_tools::ToolPreempt::Steer | plakous_tools::ToolPreempt::Notice(_)
                );
            let executions_settled = nested_executions.is_empty()
                && !state.has_active_nested()
                && state.completed_is_empty();
            // Nested execution keeps its own budget. Once it settles, bound
            // delivery acknowledgements afresh rather than joining on an old
            // outer observation deadline that expired inside the nested call.
            if executions_settled
                && (!nested_was_settled || (steer_pending && !steer_settlement_started))
            {
                response_deadline.as_mut().reset(
                    tokio::time::Instant::now()
                        + observation.timeout(operation_timeout, steer_pending),
                );
                steer_settlement_started = steer_pending;
            }
            nested_was_settled = executions_settled;
            let response_ready = state.pending_is_empty()
                && nested_executions.is_empty()
                && state.completed_is_empty()
                && state.deliveries_are_empty()
                && (matches!(observation, LocalCompositionObservation::Wait { .. })
                    || !state.has_active_nested());
            tokio::select! {
                biased;
                () = self.shutdown.cancelled() => {
                    command.cancellation.cancel();
                    break Err(cancelled_error());
                }
                () = command.cancellation.cancelled() => {
                    break Err(cancelled_error());
                }
                control = self.control_receiver.recv() => {
                    let Some(control) = control else {
                        command.cancellation.cancel();
                        break Err(cancelled_error());
                    };
                    Box::pin(self.handle_control(Some(command), control)).await;
                }
                () = &mut response_deadline, if executions_settled && (!steer_pending || steer_settlement_started) => break Err(local_composition_started_error(
                    observation.deadline_error(),
                )),
                response = observation.receive(), if response_ready => break response.resolve(state),
                // Never drop a nested execution: the driver retains mutation
                // settlement authority and drains an admitted read-only group.
                () = async {}, if steer_pending
                    && executions_settled && state.deliveries_are_empty() => {
                    break Ok(LocalCompositionResponse::Yielded {
                        cell_id: observation.cell_id().to_owned(),
                        items: self.tool_preempt.borrow().notice_reason().map(|reason| LocalCompositionContentItem::Text(reason.to_owned())).into_iter().collect(),
                    });
                }
                _ = job_changes.changed() => self.preempt_for_job_terminals(),
                delivery = state.next_delivery(), if !state.deliveries_are_empty() => {
                    // Every receipt is the same instruction to the driver:
                    // carry on. A refusal means the cell no longer owns that
                    // request and a proved non-delivery means it never saw it,
                    // so in both the committed child outcome stands, nothing is
                    // replayed and no effect is repeated. Only an
                    // acknowledgement of a genuine pending promise that can be
                    // neither proved nor ruled out ends the turn.
                    if let Some(Err(error)) = delivery {
                        break Err(error);
                    }
                }
                event = events.receive() => {
                    let event = match event {
                        Ok(event) => event,
                        Err(loss) => break Err(local_composition_session_loss_error(state, &loss)),
                    };
                    if let Err(error) = self.handle_local_composition_broker_event(state, event) {
                        break Err(error);
                    }
                },
                () = async {}, if !steer_pending
                    && state.pending_front().is_some_and(|pending| {
                    self.tools.definitions().iter().find(|definition| definition.name().as_str() == pending.call.tool_name).is_some_and(|definition| definition.effect() == ToolEffect::ReadOnly)
                        || (!state.has_active_nested() && state.completed_is_empty())
                }) => {
                    let read_only = state.pending_front().is_some_and(|pending| {
                        self.tools.definitions().iter().find(|definition| definition.name().as_str() == pending.call.tool_name).is_some_and(|definition| definition.effect() == ToolEffect::ReadOnly)
                    });
                    if read_only {
                        let Some(pending) = state.take_pending() else {
                            break Err(AgentError::new(AgentErrorKind::Internal, "nested scheduler queue changed during admission"));
                        };
                        match self.start_local_composition_nested_read_only(command, attempt_id, state, pending).await {
                            Ok(execution) => {
                                state.execution_started();
                                nested_executions.push(execution);
                            }
                            Err((pending, error)) => {
                                if let Err(delivery) = self.buffer_local_composition_nested_rejection(command, state, pending, error).await { break Err(delivery); }
                            }
                        }
                    } else {
                        nested_was_settled = false;
                        if let Err(error) = Box::pin(self.dispatch_next_local_composition_nested(command, attempt_id, state)).await {
                            break Err(error);
                        }
                    }
                }
                completion = nested_executions.next(), if !nested_executions.is_empty() => {
                    if let Some(completion) = completion
                        && let Err(error) = self.buffer_local_composition_nested_read_only(command, state, completion).await
                    { break Err(error); }
                }
            }
        }
    }
}
