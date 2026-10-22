//! Concrete tool execution and dispatch observation.

use crate::runtime::checkpoint::owner::StartAuthority;
use crate::runtime::driver::{
    ActorToolDispatchObserver, IndexedReadOnlyRequest, IndexedToolExecution,
    ProgressForwardingGuard, ToolDispatchTelemetry,
};
use crate::runtime::observation::{
    ObservationClip, canonical_tool_result_item, clip_tool_result, normalized_tool_failure,
    tool_call_observation, tool_result_content_bytes,
};
use crate::runtime::terminal::{
    actor_panic_error, cancelled_error, lock_mutex, tool_terminal_status,
};
use crate::runtime::{ToolExecutionResult, TurnShared};
use crate::tool_runtime::{
    ToolDispatchObserver, ToolDispatchObserverFuture, ToolDispatchPermit, ToolExecutionControl,
};
use crate::{
    AgentError, AgentErrorKind, AgentEventKind, EventContext, EventPipeline, EventToolProgress,
    TelemetryObservation, TelemetryObservationKind, TelemetryTerminalStatus, ToolAuditIdentity,
    ToolCallObservation, ToolCallerLineageView, ToolProgressSummary,
};
use futures_util::FutureExt;
use plakous_model::{AttemptId, CallId, CancellationToken, ConversationItem, EntryId, TurnId};
use plakous_tools::{
    ProgressReceiver, ProgressSink, ToolProgress, ToolRuntimeError, ToolRuntimeErrorKind,
};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
impl ProgressForwardingGuard {
    pub(in crate::runtime) fn close(&self) {
        self.0.close();
    }
}

impl Drop for ProgressForwardingGuard {
    fn drop(&mut self) {
        self.close();
    }
}

impl ActorToolDispatchObserver {
    #[allow(
        clippy::too_many_arguments,
        reason = "exact dispatch observation and authority inputs"
    )]
    pub(in crate::runtime) fn new(
        authority: StartAuthority,
        cancellation: CancellationToken,
        shared: Arc<TurnShared>,
        event_context: EventContext,
        audit_identity: ToolAuditIdentity,
        lineage: ToolCallerLineageView,
        observation: ToolCallObservation,
        telemetry: ToolDispatchTelemetry,
        dispatch_witness: Arc<AtomicBool>,
    ) -> Self {
        Self {
            authority,
            cancellation,
            shared,
            event_context,
            audit_identity,
            lineage,
            observation,
            telemetry: telemetry.emitter,
            telemetry_context: telemetry.context,
            clock: telemetry.clock,
            started: Mutex::new(None),
            dispatch_witness,
            error: Mutex::new(None),
        }
    }

    pub(in crate::runtime) fn observe_started(&self) -> Result<(), AgentError> {
        {
            let mut started = lock_mutex(&self.started);
            if started.is_some() {
                return Err(AgentError::new(
                    AgentErrorKind::Internal,
                    "tool dispatch observer was invoked more than once",
                ));
            }
            *started = Some(tokio::time::Instant::now());
        }
        if let Err(error) = self.shared.begin_local_tool_dispatch(
            self.event_context,
            self.audit_identity.clone(),
            self.lineage.clone(),
            self.observation.clone(),
        ) {
            *lock_mutex(&self.started) = None;
            return Err(error);
        }
        self.dispatch_witness.store(true, Ordering::Release);
        self.telemetry.emit(TelemetryObservation::new(
            self.telemetry_context,
            self.clock.now(),
            TelemetryObservationKind::ToolStarted {
                tool: self
                    .audit_identity
                    .exposed_name()
                    .unwrap_or_else(|| self.audit_identity.canonical_name())
                    .into(),
            },
        ));
        Ok(())
    }

    pub(in crate::runtime) fn emit_finished(
        &self,
        status: TelemetryTerminalStatus,
        output_bytes: u64,
    ) -> bool {
        let Some(started) = lock_mutex(&self.started).take() else {
            return false;
        };
        let elapsed = started.elapsed();
        self.telemetry.emit(TelemetryObservation::new(
            self.telemetry_context,
            self.clock.now(),
            TelemetryObservationKind::ToolFinished {
                elapsed,
                status,
                output_bytes,
            },
        ));
        self.shared.record_tool_telemetry(elapsed);
        true
    }

    pub(in crate::runtime) fn take_error(&self) -> Option<AgentError> {
        lock_mutex(&self.error).take()
    }
}

impl ToolDispatchObserver for ActorToolDispatchObserver {
    fn dispatch_started(&self) -> ToolDispatchObserverFuture<'_> {
        Box::pin(async move {
            let guard = self
                .authority
                .admit()
                .await
                .map_err(|error| self.reject_start(error))?;
            Ok(ToolDispatchPermit::new(move || {
                let _guard = guard;
                let result = self.authority.check().and_then(|()| {
                    Self::check_controls(&self.cancellation)?;
                    self.observe_started()
                });
                result.map_err(|error| self.reject_start(error))
            }))
        })
    }
}

impl ActorToolDispatchObserver {
    pub(in crate::runtime) fn check_controls(
        cancellation: &CancellationToken,
    ) -> Result<(), AgentError> {
        if cancellation.is_cancelled() {
            return Err(cancelled_error());
        }
        Ok(())
    }

    pub(in crate::runtime) fn reject_start(&self, error: AgentError) -> ToolRuntimeError {
        *lock_mutex(&self.error) = Some(error);
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::Execution,
            "agent rejected tool dispatch admission",
        )
    }
}

pub(super) async fn execute_indexed_read_only(
    request: IndexedReadOnlyRequest,
) -> IndexedToolExecution {
    let operation_id = request.operation_id;
    let index = request.index;
    let call = request.call.clone();
    let result_entry_id = request.result_entry_id;
    let audit_identity = request.audit_identity.clone();
    let lineage = request.lineage.clone();
    let dispatch_witness = Arc::clone(&request.dispatch_witness);
    let process_session_update = Arc::clone(&request.process_session_update);
    let result = Box::pin(AssertUnwindSafe(execute_read_only_tool(request)).catch_unwind())
        .await
        .unwrap_or_else(|_| Err(actor_panic_error()));
    IndexedToolExecution {
        operation_id,
        index,
        call,
        result_entry_id,
        audit_identity,
        lineage,
        dispatched: dispatch_witness.load(Ordering::Acquire),
        process_session_update: lock_mutex(&process_session_update).clone(),
        result,
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "execution, progress forwarding, terminal drain, and telemetry ordering remain visible together"
)]
async fn execute_read_only_tool(
    request: IndexedReadOnlyRequest,
) -> Result<ToolExecutionResult, AgentError> {
    let IndexedReadOnlyRequest {
        authority,
        operation_id: _,
        tools,
        tool_presentation,
        invocation,
        projection_receipt,
        authorized,
        audit_identity,
        lineage,
        index: _,
        call,
        result_entry_id: _,
        cancellation,
        shutdown,
        preempt,
        preempt_guard: _preempt_guard,
        clock,
        progress_capacity,
        events,
        turn_id,
        attempt_id,
        telemetry,
        telemetry_context,
        turn_shared,
        dispatch_witness,
        process_session_update: process_session_update_slot,
    } = request;
    let call_id = call.call_id();
    let (progress, progress_receiver) =
        ProgressSink::channel(progress_capacity, call.call_id(), Arc::clone(&clock))
            .map_err(|error| AgentError::from_tool(&error))?;
    let progress_guard = ProgressForwardingGuard(progress.clone());
    let dispatch_observer = Arc::new(ActorToolDispatchObserver::new(
        authority,
        cancellation.clone(),
        turn_shared,
        EventContext::call(turn_id, attempt_id, call_id),
        audit_identity.clone(),
        lineage.clone(),
        tool_call_observation(&call),
        ToolDispatchTelemetry {
            emitter: telemetry,
            context: telemetry_context,
            clock: Arc::clone(&clock),
        },
        dispatch_witness,
    ));
    let control =
        ToolExecutionControl::new(cancellation.clone(), None, Arc::clone(&clock), progress)
            .with_preempt(preempt)
            .with_dispatch_observer(dispatch_observer.clone());
    let mut execution = Box::pin(tools.execute(authorized, control));
    let mut progress_open = true;
    let outcome = loop {
        tokio::select! {
            progress = progress_receiver.recv(), if progress_open => {
                match progress {
                    Some(progress) => {
                        publish_tool_progress(
                            &events,
                            turn_id,
                            attempt_id,
                            call_id,
                            &audit_identity,
                            &lineage,
                            &progress,
                        );
                    }
                    None => progress_open = false,
                }
            }
            outcome = &mut execution => break outcome,
        }
    };
    progress_guard.close();
    drain_tool_progress(
        &progress_receiver,
        &events,
        turn_id,
        attempt_id,
        call_id,
        &audit_identity,
        &lineage,
    )
    .await;
    let execution_outcome = outcome;
    if let Ok(outcome) = &execution_outcome {
        dispatch_observer.observe_job_outcome(outcome);
    }
    let process_session_update = execution_outcome
        .as_ref()
        .ok()
        .and_then(plakous_tools::internal::process_session_update);
    *lock_mutex(&process_session_update_slot) = process_session_update;
    let (result, presentation_subject) = match &execution_outcome {
        Ok(outcome) => {
            let raised = tool_presentation
                .raise(projection_receipt.as_ref(), outcome.clone())
                .map_err(|error| AgentError::from_tool(&error))?;
            let presentation_subject = raised.presentation_subject().map(str::to_owned);
            let result = raised
                .to_model_result(&invocation)
                .map_err(|error| AgentError::from_tool(&error))?;
            (result, presentation_subject)
        }
        Err(error) if error.kind() == ToolRuntimeErrorKind::Cancelled => {
            return Err(cancelled_error());
        }
        Err(error) => (normalized_tool_failure(&call, error)?, None),
    };
    let output_bytes = tool_result_content_bytes(&result);
    let dispatched = dispatch_observer.emit_finished(
        tool_terminal_status(&execution_outcome, shutdown.is_cancelled()),
        output_bytes,
    );
    if let Some(error) = dispatch_observer.take_error() {
        return Err(error);
    }
    Ok(
        ToolExecutionResult::new(result, dispatched)
            .with_presentation_subject(presentation_subject),
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "the arguments are the exact published progress correlation tuple"
)]
pub(super) fn publish_tool_progress(
    events: &EventPipeline,
    turn_id: TurnId,
    attempt_id: AttemptId,
    call_id: CallId,
    audit_identity: &ToolAuditIdentity,
    lineage: &ToolCallerLineageView,
    progress: &ToolProgress,
) {
    let summary = ToolProgressSummary::from_progress(progress);
    let _ = events.publish(
        EventContext::call(turn_id, attempt_id, call_id),
        AgentEventKind::ToolProgress {
            identity: audit_identity.clone(),
            lineage: lineage.clone(),
            summary,
            progress: Some(EventToolProgress::from(progress)),
        },
    );
}

#[allow(
    clippy::too_many_arguments,
    reason = "the arguments are the exact published progress correlation tuple"
)]
pub(super) async fn drain_tool_progress(
    receiver: &ProgressReceiver,
    events: &EventPipeline,
    turn_id: TurnId,
    attempt_id: AttemptId,
    call_id: CallId,
    audit_identity: &ToolAuditIdentity,
    lineage: &ToolCallerLineageView,
) {
    while let Some(progress) = receiver.recv().await {
        publish_tool_progress(
            events,
            turn_id,
            attempt_id,
            call_id,
            audit_identity,
            lineage,
            &progress,
        );
    }
}

pub(in crate::runtime) fn canonicalize_tool_execution(
    result_entry_id: EntryId,
    execution: ToolExecutionResult,
    budget: &mut crate::runtime::observation::budget::ObservationBudget,
) -> Result<ConversationItem, AgentError> {
    Ok(canonicalize_observed_tool_execution(result_entry_id, execution, budget)?.0)
}

/// [`canonicalize_tool_execution`], and what the budget removed doing it.
///
/// Every canonicalizing site that can name a call should use this form and
/// publish. That is nearly all of them: the read-only and rejection paths in
/// `driver/tools.rs` and `driver/turn.rs`, the mutating-effect path in
/// `driver/effects.rs`, the outer composition sites, and the durable tool-batch
/// reconcile in `continuation.rs`.
///
/// Two exceptions, both because the caller cannot name a call. The nested
/// composition child's completion carries no attempt id, and this event is
/// call-scoped, so the composed answer is announced at the outer site instead.
/// And [`canonicalize_tool_execution`] remains for callers with no event stream
/// at all.
///
/// **Accepted: a genuine reopen re-announces.** The durable reconcile publishes
/// whenever it canonicalizes journaled evidence, so reopening a session repeats
/// a clip note the operator already saw. Telling an ordinary round from a replay
/// needs the frame to record that it has announced. Until it can, a repeated
/// true note is better than a missing one: the failure this replaced was the
/// note never appearing at all, on every turn, for as long as the feature
/// existed.
pub(in crate::runtime) fn canonicalize_observed_tool_execution(
    result_entry_id: EntryId,
    execution: ToolExecutionResult,
    budget: &mut crate::runtime::observation::budget::ObservationBudget,
) -> Result<(ConversationItem, Option<ObservationClip>), AgentError> {
    let presentation_subject = execution.presentation_subject.clone();
    let (result, clip) = clip_tool_result(execution.result, budget)?;
    let item = canonical_tool_result_item(result_entry_id, result)?
        .with_presentation_subject(presentation_subject.as_deref());
    Ok((item, clip))
}

/// Announces what the observation budget removed from one tool result.
///
/// Silent when nothing was dropped: a result that fits is not an event. The
/// context is the call, not the turn, so a surface can attach the note to the
/// tool row whose output was cut rather than to the turn as a whole.
pub(in crate::runtime) fn publish_observation_clip(
    events: &EventPipeline,
    turn_id: TurnId,
    attempt_id: AttemptId,
    call_id: CallId,
    name: &plakous_model::ToolName,
    clip: Option<ObservationClip>,
) {
    let Some(clip) = clip else {
        return;
    };
    let _ = events.publish(
        EventContext::call(turn_id, attempt_id, call_id),
        AgentEventKind::ObservationClipped {
            name: Arc::from(name.as_str()),
            bytes_kept: clip.bytes_kept,
            bytes_dropped: clip.bytes_dropped,
            bound: clip.bound,
        },
    );
}

impl ActorToolDispatchObserver {
    pub(super) fn observe_job_outcome(&self, outcome: &plakous_tools::ToolOutcome) {
        observe_job_outcome(
            self.lineage.class() != crate::ToolCallerClass::Direct,
            outcome,
            |kind| {
                self.telemetry.emit(TelemetryObservation::new(
                    self.telemetry_context,
                    self.clock.now(),
                    kind,
                ));
            },
        );
    }
}

fn observe_job_outcome(
    nested: bool,
    outcome: &plakous_tools::ToolOutcome,
    mut emit: impl FnMut(TelemetryObservationKind),
) {
    use crate::{JobHintSource, JobPreemption, JobTool};
    use plakous_tools::internal::{
        ProcessHintSource, ProcessOperationKind, ProcessTelemetry, process_telemetry,
    };
    let tool = |kind| match kind {
        ProcessOperationKind::Start => JobTool::Start,
        ProcessOperationKind::Poll => JobTool::Poll,
        ProcessOperationKind::WriteWait => JobTool::WriteWait,
        ProcessOperationKind::Await => JobTool::Await,
    };
    match process_telemetry(outcome) {
        Some(ProcessTelemetry::Promoted {
            operation,
            hint_source,
            hint_ms,
            preempt,
        }) => {
            emit(TelemetryObservationKind::JobPromoted {
                tool: tool(operation),
                nested,
                hint_source: match hint_source {
                    ProcessHintSource::Default => JobHintSource::Default,
                    ProcessHintSource::ModelHint => JobHintSource::ModelHint,
                    ProcessHintSource::Background => JobHintSource::Background,
                },
                hint_ms,
            });
            if preempt != plakous_tools::ToolPreempt::None {
                emit(TelemetryObservationKind::JobPreempted {
                    kind: JobPreemption::Promoted,
                });
            }
        }
        Some(ProcessTelemetry::Waited { operation, yielded }) => {
            emit(TelemetryObservationKind::JobWaited {
                tool: tool(operation),
            });
            if yielded {
                emit(TelemetryObservationKind::JobPreempted {
                    kind: JobPreemption::WaitYielded,
                });
            }
        }
        None => (),
    }
}

#[cfg(test)]
mod job_telemetry_tests {
    use super::*;
    use crate::{JobHintSource, JobPreemption, JobTool};
    use plakous_tools::internal::{
        ProcessHintSource, ProcessOperationKind, ProcessTelemetry, with_process_telemetry,
    };
    use plakous_tools::{ToolOutcome, ToolPreempt};

    #[test]
    fn telemetry_classification_comes_from_neutral_carrier() -> Result<(), ToolRuntimeError> {
        let outcome = with_process_telemetry(
            ToolOutcome::success_text("not retained")?,
            ProcessTelemetry::Waited {
                operation: ProcessOperationKind::Poll,
                yielded: false,
            },
        );
        let mut events = Vec::new();
        observe_job_outcome(false, &outcome, |kind| events.push(kind));
        assert_eq!(
            events,
            [TelemetryObservationKind::JobWaited {
                tool: JobTool::Poll
            }]
        );
        Ok(())
    }

    #[test]
    fn promotions_observe_effective_hints_and_preemption_without_content()
    -> Result<(), ToolRuntimeError> {
        for (source, expected) in [
            (ProcessHintSource::Default, JobHintSource::Default),
            (ProcessHintSource::ModelHint, JobHintSource::ModelHint),
            (ProcessHintSource::Background, JobHintSource::Background),
        ] {
            for nested in [false, true] {
                for preempt in [
                    ToolPreempt::None,
                    ToolPreempt::Steer,
                    ToolPreempt::Background,
                ] {
                    let outcome = with_process_telemetry(
                        ToolOutcome::success_text("not retained")?,
                        ProcessTelemetry::Promoted {
                            operation: ProcessOperationKind::Start,
                            hint_source: source,
                            hint_ms: 600_000,
                            preempt,
                        },
                    );
                    let mut events = Vec::new();
                    observe_job_outcome(nested, &outcome, |kind| events.push(kind));
                    assert!(
                        matches!(events.first(), Some(TelemetryObservationKind::JobPromoted {nested: actual, hint_source, hint_ms:600_000, tool:JobTool::Start}) if *actual == nested && *hint_source == expected)
                    );
                    assert_eq!(
                        events.len(),
                        if preempt == ToolPreempt::None { 1 } else { 2 }
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn waits_observe_yielding_from_the_tool_carrier() -> Result<(), ToolRuntimeError> {
        let outcome = ToolOutcome::success_text("not retained")?.with_wait_observation(true);
        let mut events = Vec::new();
        observe_job_outcome(false, &outcome, |kind| events.push(kind));
        assert_eq!(
            events,
            [
                TelemetryObservationKind::JobWaited {
                    tool: JobTool::Await
                },
                TelemetryObservationKind::JobPreempted {
                    kind: JobPreemption::WaitYielded
                }
            ]
        );
        Ok(())
    }
}
