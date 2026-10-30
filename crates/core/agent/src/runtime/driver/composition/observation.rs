//! Observe an executing or continuing cell without relinquishing nested settlement authority.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the composition driver that shares its vocabulary"
)]

use super::*;
use crate::local_composition::{
    LocalCompositionEventReceiver, LocalCompositionFuture, LocalCompositionSession,
    LocalCompositionSessionFuture,
};

const EXPEDITED_OBSERVATION_BOUND: Duration = Duration::from_secs(5);

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

    fn yielded(&self, notice_reason: Option<String>) -> LocalCompositionResponse {
        LocalCompositionResponse::Yielded {
            cell_id: self.cell_id().to_owned(),
            items: notice_reason
                .map(LocalCompositionContentItem::Text)
                .into_iter()
                .collect(),
        }
    }

    async fn receive(
        &mut self,
        resume_in_flight: &mut bool,
        yield_requested: bool,
        notice_reason: Option<String>,
        wait_resumes_cell: bool,
        poll_response: bool,
    ) -> ObservedResponse {
        if yield_requested && !*resume_in_flight {
            let yielded = self.yielded(notice_reason);
            if poll_response && let Self::Execute { response, .. } = self {
                // Execute has already started. Prefer its buffered terminal;
                // unlike a wait, polling it cannot issue a new Resume.
                return std::future::poll_fn(|cx| {
                    std::task::Poll::Ready(match response.as_mut().poll(cx) {
                        std::task::Poll::Ready(result) => ObservedResponse::Execute(result),
                        std::task::Poll::Pending => ObservedResponse::Yielded(yielded.clone()),
                    })
                })
                .await;
            }
            if poll_response
                && !wait_resumes_cell
                && let Self::Wait { response, .. } = self
            {
                // A watch-only observation cannot resume anything, so it is
                // safe to prefer its terminal before yielding to the notice.
                return std::future::poll_fn(|cx| {
                    std::task::Poll::Ready(match response.as_mut().poll(cx) {
                        std::task::Poll::Ready(result) => ObservedResponse::Wait(result),
                        std::task::Poll::Pending => ObservedResponse::Yielded(yielded.clone()),
                    })
                })
                .await;
            }
            return ObservedResponse::Yielded(yielded);
        }
        match self {
            Self::Execute { response, .. } => ObservedResponse::Execute(response.await),
            Self::Wait { response, .. } => {
                // Polling may issue Resume before returning Pending. From this
                // point the session retains that resume even if this
                // observation later yields synthetically after expedite.
                *resume_in_flight = wait_resumes_cell;
                ObservedResponse::Wait(response.await)
            }
        }
    }
}

enum ObservedResponse {
    Yielded(LocalCompositionResponse),
    Execute(Result<LocalCompositionResponse, LocalCompositionSessionLoss>),
    Wait(Result<LocalCompositionWaitOutcome, String>),
}

impl ObservedResponse {
    fn with_notice(mut self, reason: Option<String>) -> Self {
        let response = match &mut self {
            Self::Execute(Ok(response))
            | Self::Wait(Ok(LocalCompositionWaitOutcome::Live(response))) => Some(response),
            _ => None,
        };
        if let Some(LocalCompositionResponse::Yielded { items, .. }) = response
            && let Some(reason) = reason
        {
            items.push(LocalCompositionContentItem::Text(reason));
        }
        self
    }

    fn resolve(self, state: &mut CompositionOwner) -> Result<LocalCompositionResponse, AgentError> {
        match self {
            Self::Yielded(response) => Ok(response),
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
        io: (
            &dyn LocalCompositionSession,
            &mut dyn LocalCompositionEventReceiver,
        ),
        nested_executions: &mut FuturesUnordered<LocalCompositionNestedFuture>,
        observation: &mut LocalCompositionObservation<'_>,
    ) -> Result<LocalCompositionResponse, AgentError> {
        let (session, events) = io;
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
        let mut resume_in_flight = false;
        let mut expedited = false;
        let mut expedited_deadline_started = false;
        let wait_resumes_cell = state.integration().runtime_contract().wait_resumes_cell;
        let mut job_changes = self.job_sessions.job_terminal_changes();
        job_changes.mark_changed();
        loop {
            let steer_pending = state.integration().runtime_contract().supports_yield
                && matches!(
                    *self.tool_preempt.borrow(),
                    plakous_tools::ToolPreempt::Steer | plakous_tools::ToolPreempt::Notice(_)
                );
            if steer_pending && resume_in_flight && !expedited {
                let deadline = tokio::time::Instant::now() + EXPEDITED_OBSERVATION_BOUND;
                let expedited_result = tokio::time::timeout_at(
                    deadline,
                    session.expedite(observation.cell_id().to_owned()),
                )
                .await;
                if let Ok(result) = expedited_result {
                    result.map_err(|loss| local_composition_session_loss_error(state, &loss))?;
                }
                expedited = true;
            }
            let executions_settled = nested_executions.is_empty()
                && !state.has_active_nested()
                && state.completed_is_empty();
            // Nested execution keeps its own budget. Once it settles, bound
            // delivery acknowledgements afresh rather than joining on an old
            // outer observation deadline that expired inside the nested call.
            if executions_settled
                && (!nested_was_settled
                    || (steer_pending && !steer_settlement_started && !expedited))
            {
                response_deadline.as_mut().reset(
                    tokio::time::Instant::now()
                        + observation.timeout(operation_timeout, steer_pending && !expedited),
                );
                steer_settlement_started = steer_pending;
            }
            nested_was_settled = executions_settled;
            if expedited
                && executions_settled
                && state.deliveries_are_empty()
                && !expedited_deadline_started
            {
                response_deadline
                    .as_mut()
                    .reset(tokio::time::Instant::now() + EXPEDITED_OBSERVATION_BOUND);
                expedited_deadline_started = true;
            }
            let response_ready = state.pending_is_empty()
                && nested_executions.is_empty()
                && state.completed_is_empty()
                && state.deliveries_are_empty()
                && (matches!(observation, LocalCompositionObservation::Wait { .. })
                    || !state.has_active_nested());
            let yield_ready = steer_pending && executions_settled && state.deliveries_are_empty();
            let held_nested = expedited && yield_ready && !state.pending_is_empty();
            let deadline_ready =
                executions_settled && (!steer_pending || steer_settlement_started || expedited);
            let poll_response = response_ready || state.pending_is_empty();
            let notice_reason = self
                .tool_preempt
                .borrow()
                .notice_reason()
                .map(str::to_owned);
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
                () = &mut response_deadline, if deadline_ready => {
                    break if expedited_deadline_started {
                        Ok(observation.yielded(notice_reason))
                    } else {
                        Err(local_composition_started_error(observation.deadline_error()))
                    };
                },
                // Never drop a nested execution: the driver retains mutation
                // settlement authority and drains an admitted read-only group.
                response = async {
                    if held_nested {
                        // The pump cannot publish its answer until these held
                        // calls are delivered. Retain the resume in the session
                        // and let the next wait dispatch them instead.
                        return ObservedResponse::Yielded(observation.yielded(notice_reason.clone()));
                    }
                    if yield_ready && !resume_in_flight && poll_response
                        && matches!(observation, LocalCompositionObservation::Wait { .. })
                    {
                        match session.buffered(observation.cell_id().to_owned()).await {
                            Ok(Some(buffered)) => return ObservedResponse::Wait(Ok(buffered)).with_notice(notice_reason.clone()),
                            Err(loss) => return ObservedResponse::Wait(Err(loss)),
                            Ok(None) => {}
                        }
                    }
                    observation.receive(&mut resume_in_flight, yield_ready, notice_reason.clone(), wait_resumes_cell, poll_response).await.with_notice(notice_reason.clone())
                },
                    if response_ready || yield_ready => break response.resolve(state),
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

#[cfg(test)]
mod tests;
