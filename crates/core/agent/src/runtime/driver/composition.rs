//! Drive interpreter cells and nested tool dispatch in the actor pump.

use crate::local_composition::{
    LocalCompositionEvent, LocalCompositionExecuteRequest, LocalCompositionResponse,
    LocalCompositionSessionLoss, LocalCompositionShutdownResult, LocalCompositionTerminateResult,
    LocalCompositionWaitOutcome, LocalCompositionWaitRequest,
};
use crate::manifest::InterpreterObservation;
use crate::runtime::cleanup::{emit_shutdown_child_exit, error_with_cleanup_disposition};
use crate::runtime::composition::nested::{
    fence_pending_local_composition_nested, queue_local_composition_nested_results,
    take_local_composition_nested_waiters,
};
use crate::runtime::composition::projection::{
    clip_local_composition_update_text, local_composition_child_call_id,
    local_composition_nested_call_arguments, local_composition_nested_decision_unsupported,
    local_composition_nested_execution_owner, local_composition_nested_result_value,
    local_composition_prestart_error, local_composition_recovery_authoritative,
    local_composition_response_cell, local_composition_response_parts,
    local_composition_response_terminal, local_composition_started_error,
    local_composition_wait_response,
};
use crate::runtime::composition::record::{
    DurableLocalCompositionContinuationRequestV1, DurableLocalCompositionEvaluationRequestV2,
    encode_local_composition_continuation_request, encode_local_composition_evaluation_request,
};
use crate::runtime::composition::resources::{
    InterpreterResources, join_extracted_local_composition_owner,
    local_composition_session_loss_error, observe_local_composition_session_loss,
    take_local_composition_owner,
};
use crate::runtime::composition::{
    CompositionOwner, LocalCompositionNestedDisposition, LocalCompositionNestedFuture,
    LocalCompositionNestedReadOnlyCompletion, LocalCompositionNestedReady,
    LocalCompositionShutdownTelemetry, PendingLocalCompositionNested,
};
use crate::runtime::continuation::runtime_component;
use crate::runtime::driver::ToolDispatchOrigin;
use crate::runtime::driver::execution::{
    canonicalize_observed_tool_execution, execute_indexed_read_only, publish_observation_clip,
};
use crate::runtime::driver::{
    ActorToolDispatchObserver, AdmittedToolExecution, Driver, IndexedReadOnlyRequest,
    LocalCompositionCallResponse, LocalCompositionOuterExecution, ToolAdmission,
    ToolDispatchTelemetry,
};
use crate::runtime::observation::{
    canonical_tool_result_item, clip_tool_result, normalized_tool_failure,
    tool_result_observation_with_subject,
};
use crate::runtime::operation_record::{
    canonical_tool_operation_binding, tool_operation_terminal_state,
};
use crate::runtime::terminal::{actor_panic_error, cancelled_error};
use crate::runtime::{OperationId, PromptCommand, ToolPresentation};
use crate::{
    AgentError, AgentErrorKind, AgentEventKind, DurableOperationKind, DurableOperationState,
    EventContext, RuntimeComponentKind, SessionStoreError, StoreAdmission, ToolAuditIdentity,
    ToolCallObservation, ToolCallerLineageView,
};
use futures_util::stream::FuturesUnordered;
use futures_util::{FutureExt, StreamExt};
use plakous_model::{
    Actor, AttemptId, CallId, ContentBlock, ConversationItem, Digest, DigestDomain, EntryId,
    Provenance, ToolCallBlock, ToolCallState, ToolCorrelation, ToolEffect, ToolExecutionOwner,
    ToolResultUpdateBlock, TurnId,
};
use plakous_tools::{ToolInvocation, ToolOutcome, ToolRuntimeError, ToolRuntimeErrorKind};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;
const LOCAL_COMPOSITION_FAILURE_TAG: &str = "plakous-local-composition-failure-v1";

/// What a failed local composition records on its marker operation. It is
/// encoded as the tagged tuple `[tag, kind, message, child operation ids]`.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub(super) struct LocalCompositionFailureEvidence(String, AgentErrorKind, String, Vec<u64>);

impl LocalCompositionFailureEvidence {
    fn new(kind: AgentErrorKind, message: &str, child_operations: Vec<u64>) -> Self {
        Self(
            LOCAL_COMPOSITION_FAILURE_TAG.to_owned(),
            kind,
            message.to_owned(),
            child_operations,
        )
    }

    /// Decodes failure evidence; `None` when the bytes carry another tag.
    pub(super) fn decode(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice::<Self>(bytes)
            .ok()
            .filter(|evidence| evidence.0 == LOCAL_COMPOSITION_FAILURE_TAG)
    }

    pub(super) fn detail(&self) -> &str {
        &self.2
    }

    pub(super) fn child_operations(&self) -> &[u64] {
        &self.3
    }
}

impl Driver {
    pub(super) fn is_local_composition_invocation(&self, invocation: &ToolInvocation) -> bool {
        self.local_composition.as_ref().is_some_and(|state| {
            invocation.identity().executor_id() == &state.descriptor().marker_executor
        })
    }

    pub(super) fn is_local_composition_marker(&self, execution: &AdmittedToolExecution) -> bool {
        match execution {
            AdmittedToolExecution::ReadOnly { invocation, .. }
            | AdmittedToolExecution::Mutating { invocation, .. } => {
                self.is_local_composition_invocation(invocation)
            }
        }
    }

    /// Returns the outer marker call owning the driver's active cell, if any.
    ///
    /// Read before `exec_call_id` is cleared at the cell's terminal.
    pub(super) fn active_composition_marker_call(&self) -> Option<CallId> {
        self.local_composition
            .as_ref()
            .and_then(CompositionOwner::exec_call_id)
    }

    /// Projects the caller lineage published on this call's lifecycle events.
    ///
    /// The canonical lineage minted on the child `ToolCallBlock` supplies the
    /// cell, ordinal, and depth; the owning marker call comes from the driver
    /// rather than from canonical content.
    pub(super) fn tool_caller_lineage(&self, call: &ToolCallBlock) -> ToolCallerLineageView {
        ToolCallerLineageView::from_lineage(
            call.correlation().caller_lineage(),
            self.active_composition_marker_call(),
        )
    }

    /// Projects the caller lineage of an outer local-composition marker call.
    ///
    /// The marker owns the cell, so it never carries a parent, an ordinal, or a
    /// depth. `cell_id` is the driver's active cell, which the interpreter has
    /// not yet minted when the first `exec*` of a cell is dispatched.
    pub(super) fn local_composition_marker_lineage(&self) -> ToolCallerLineageView {
        ToolCallerLineageView::local_composition_marker(
            self.local_composition
                .as_ref()
                .and_then(CompositionOwner::cell),
        )
    }

    /// The marker lineage published on an outer marker's own `ToolCompleted`.
    ///
    /// The completion is published after the cell has been retired, so the
    /// driver's live `cell` is `None` there. The caller lineage marker must
    /// carry the cell it owned, and a `wait*` continuation's terminal is the
    /// only event that says which cell it closed, so the retired cell is used
    /// when the live one is already gone. Nothing is guessed: with no cell in
    /// either slot the lineage still carries `None`.
    pub(super) fn local_composition_marker_terminal_lineage(&self) -> ToolCallerLineageView {
        ToolCallerLineageView::local_composition_marker(
            self.local_composition
                .as_ref()
                .and_then(|state| state.cell().or(state.retired_cell())),
        )
    }

    pub(super) fn tool_dispatch_observer(
        &self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        call_id: CallId,
        audit_identity: ToolAuditIdentity,
        lineage: ToolCallerLineageView,
        observation: ToolCallObservation,
    ) -> Arc<ActorToolDispatchObserver> {
        let dispatch_witness = Arc::new(AtomicBool::new(false));
        let telemetry_context = self
            .telemetry_context
            .with_turn(command.turn_id)
            .with_attempt(attempt_id)
            .with_call(call_id);
        Arc::new(ActorToolDispatchObserver::new(
            self.start_authority(),
            command.cancellation.clone(),
            Arc::clone(&command.shared),
            EventContext::call(command.turn_id, attempt_id, call_id),
            audit_identity,
            lineage,
            observation,
            ToolDispatchTelemetry {
                emitter: Arc::clone(&self.telemetry),
                context: telemetry_context,
                clock: Arc::clone(&self.clock),
            },
            dispatch_witness,
        ))
    }

    pub(super) async fn execute_local_composition_outer(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        source_index: u32,
        call: &ToolCallBlock,
        result_entry_id: EntryId,
        execution: AdmittedToolExecution,
    ) -> Result<LocalCompositionOuterExecution, AgentError> {
        let AdmittedToolExecution::ReadOnly {
            invocation,
            projection_receipt,
            ..
        } = execution
        else {
            return Err(AgentError::new(
                AgentErrorKind::Internal,
                "local composition marker acquired mutating authority",
            ));
        };
        let binding = canonical_tool_operation_binding(source_index as usize, &invocation)?;
        let descriptor_identity = self
            .local_composition
            .as_ref()
            .ok_or_else(|| {
                AgentError::new(AgentErrorKind::Internal, "local composition is absent")
            })?
            .descriptor()
            .identity;
        let integration = Arc::clone(
            self.local_composition
                .as_ref()
                .ok_or_else(|| {
                    AgentError::new(AgentErrorKind::Internal, "local composition is absent")
                })?
                .integration(),
        );
        let is_exec = call.exposed_name().as_str()
            == self
                .local_composition
                .as_ref()
                .ok_or_else(|| {
                    AgentError::new(AgentErrorKind::Internal, "local composition is absent")
                })?
                .descriptor()
                .execute_name
                .as_ref();
        let (operation_kind, request_bytes) = if is_exec {
            let source_digest = integration
                .parse_execute(invocation.arguments().value().as_ref().clone())
                .ok()
                .and_then(|parsed| {
                    Digest::sha256(
                        DigestDomain::Blob,
                        1,
                        &[
                            b"plakous-agent/local-composition-source-v1",
                            parsed.source.as_bytes(),
                        ],
                    )
                    .ok()
                });
            (
                DurableOperationKind::CompositionEvaluation {
                    turn_id: command.turn_id,
                    attempt_id,
                    call_id: call.call_id(),
                    source_index,
                },
                encode_local_composition_evaluation_request(
                    &DurableLocalCompositionEvaluationRequestV2 {
                        version: 2,
                        descriptor_identity,
                        host_identity: integration.environment_identity(),
                        call: call.clone(),
                        arguments_digest: invocation.arguments().digest(),
                        source_digest,
                        result_entry_id,
                    },
                )?,
            )
        } else {
            let evaluation_operation_id = self
                .local_composition
                .as_ref()
                .and_then(CompositionOwner::evaluation_operation)
                .ok_or_else(|| {
                    AgentError::new(
                        AgentErrorKind::InvalidInput,
                        "local composition control has no active owning evaluation",
                    )
                })?
                .0;
            (
                DurableOperationKind::CompositionContinuation {
                    turn_id: command.turn_id,
                    attempt_id,
                    call_id: call.call_id(),
                    source_index,
                    evaluation_operation_id,
                },
                encode_local_composition_continuation_request(
                    &DurableLocalCompositionContinuationRequestV1 {
                        version: 1,
                        descriptor_identity,
                        host_identity: integration.environment_identity(),
                        evaluation_operation_id,
                        call: call.clone(),
                        arguments_digest: invocation.arguments().digest(),
                        result_entry_id,
                    },
                )?,
            )
        };
        let operation_id = self
            .prepare_operation(
                StoreAdmission::Ordinary,
                command.turn_id,
                operation_kind,
                binding,
                Some(request_bytes),
                None,
            )
            .await?;
        let mut local_composition = self.local_composition.extract()?;
        let response = AssertUnwindSafe(Box::pin(self.drive_local_composition_call(
            command,
            attempt_id,
            call,
            &invocation,
            operation_id,
            &mut local_composition,
        )))
        .catch_unwind()
        .await
        .unwrap_or_else(|_| Err(actor_panic_error()));
        let mut child_operations = local_composition.take_child_operations();
        child_operations.sort_by_key(|operation| operation.0);
        self.local_composition.restore(local_composition);
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                let state = self.operation_state(operation_id)?;
                let evidence = runtime_component(
                    RuntimeComponentKind::Operation,
                    serde_json::to_vec(&LocalCompositionFailureEvidence::new(
                        error.kind(),
                        error.message(),
                        child_operations
                            .iter()
                            .map(|operation| operation.0)
                            .collect(),
                    ))
                    .map_err(|_| {
                        AgentError::new(
                            AgentErrorKind::Internal,
                            "local composition failure evidence could not be captured",
                        )
                    })?,
                )?;
                match state {
                    DurableOperationState::Prepared => {
                        self.transition_operation(
                            command.turn_id,
                            operation_id,
                            DurableOperationState::ProvenNotDispatched(evidence),
                        )
                        .await?;
                        if error.kind() == AgentErrorKind::InvalidInput {
                            let tool_error = ToolRuntimeError::new(
                                ToolRuntimeErrorKind::InvalidArguments,
                                error.message(),
                            );
                            let result = normalized_tool_failure(call, &tool_error)?;
                            let (result, clip) =
                                clip_tool_result(result, &mut self.observation_budget)?;
                            publish_observation_clip(
                                &command.shared.events,
                                command.turn_id,
                                attempt_id,
                                call.call_id(),
                                call.exposed_name(),
                                clip,
                            );
                            return Ok(LocalCompositionOuterExecution {
                                item: canonical_tool_result_item(result_entry_id, result)?,
                                dispatched: 0,
                                operations: vec![operation_id],
                            });
                        }
                        self.finalize_operation(command.turn_id, operation_id)
                            .await?;
                    }
                    DurableOperationState::Dispatched | DurableOperationState::LocatorBound(_)
                        if error.kind() == AgentErrorKind::Cancelled =>
                    {
                        // The driver only surfaces a plain `Cancelled` here after the
                        // cell was terminated, every nested child joined, and the
                        // session shut down with a joined acknowledgement; a lost
                        // acknowledgement already widened the error to
                        // `OutcomeUnknown`. The marker's outcome is therefore known:
                        // cancelled, with every observed child terminal retained.
                        self.transition_operation(
                            command.turn_id,
                            operation_id,
                            DurableOperationState::Completed(evidence),
                        )
                        .await?;
                        if is_exec && let Some(state) = self.local_composition.as_mut() {
                            state.clear_evaluation();
                        }
                    }
                    DurableOperationState::Dispatched | DurableOperationState::LocatorBound(_) => {
                        self.transition_operation(
                            command.turn_id,
                            operation_id,
                            DurableOperationState::OutcomeUnknown(evidence),
                        )
                        .await?;
                    }
                    _ => {}
                }
                return Err(error);
            }
        };
        let LocalCompositionCallResponse {
            response,
            max_output_tokens,
            wall_time,
        } = response;
        let updates = self
            .local_composition
            .as_mut()
            .map_or_else(Vec::new, CompositionOwner::take_updates);
        let integration = self.local_composition.as_ref().ok_or_else(|| {
            AgentError::new(
                AgentErrorKind::Internal,
                "local composition owner disappeared",
            )
        })?;
        let response_parts = local_composition_response_parts(
            &response,
            max_output_tokens,
            wall_time,
            integration.integration().as_ref(),
        )?;
        let evidence = runtime_component(
            RuntimeComponentKind::Operation,
            serde_json::to_vec(&(
                "plakous-local-composition-cell-v2",
                local_composition_response_cell(&response),
                self.local_composition
                    .as_ref()
                    .and_then(CompositionOwner::source_digest),
                self.local_composition
                    .as_ref()
                    .map_or(0, CompositionOwner::nested_sequence),
                child_operations
                    .iter()
                    .map(|operation| operation.0)
                    .collect::<Vec<_>>(),
                if local_composition_response_terminal(&response) {
                    "cleanup-required"
                } else {
                    "active"
                },
                &updates,
                &response_parts,
            ))
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "local composition cell evidence could not be captured",
                )
            })?,
        )?;
        if is_exec && !local_composition_response_terminal(&response) {
            self.transition_operation(
                command.turn_id,
                operation_id,
                DurableOperationState::LocatorBound(evidence),
            )
            .await?;
        } else {
            self.transition_operation(
                command.turn_id,
                operation_id,
                DurableOperationState::Completed(evidence),
            )
            .await?;
        }
        if is_exec
            && local_composition_response_terminal(&response)
            && let Some(state) = self.local_composition.as_mut()
        {
            state.clear_evaluation();
        }
        let outcome =
            ToolOutcome::success(response_parts).map_err(|error| AgentError::from_tool(&error))?;
        let result = self
            .tool_presentation
            .raise(projection_receipt.as_ref(), outcome)
            .and_then(|outcome| outcome.to_model_result(&invocation))
            .map_err(|error| AgentError::from_tool(&error))?;
        // Code Mode cuts here too, and its results are the largest in the
        // product: a composition answer carries every nested call's output.
        let (result, clip) = clip_tool_result(result, &mut self.observation_budget)?;
        publish_observation_clip(
            &command.shared.events,
            command.turn_id,
            attempt_id,
            call.call_id(),
            call.exposed_name(),
            clip,
        );
        let mut blocks = updates
            .into_iter()
            .map(ContentBlock::ToolResultUpdate)
            .collect::<Vec<_>>();
        blocks.push(ContentBlock::ToolResult(result));
        let item = ConversationItem::new(
            result_entry_id,
            Actor::Tool,
            blocks,
            Provenance::ToolExecution,
        )
        .map_err(|_| {
            AgentError::new(
                AgentErrorKind::Tool,
                "canonical local composition result item is invalid",
            )
        })?;
        let mut operations = child_operations;
        if !is_exec || local_composition_response_terminal(&response) {
            operations.push(operation_id);
        }
        if !is_exec
            && local_composition_response_terminal(&response)
            && let Some(evaluation) = self
                .local_composition
                .as_mut()
                .and_then(CompositionOwner::take_evaluation)
        {
            self.transition_operation(
                command.turn_id,
                evaluation,
                DurableOperationState::Completed(runtime_component(
                    RuntimeComponentKind::Operation,
                    b"local-composition-cell-terminal-and-cleanup-required".to_vec(),
                )?),
            )
            .await?;
            operations.push(evaluation);
            if let Some(state) = self.local_composition.as_mut() {
                state.clear_execution_linkage();
            }
        }
        Ok(LocalCompositionOuterExecution {
            item,
            dispatched: 1,
            operations,
        })
    }

    async fn drive_local_composition_call(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        call: &ToolCallBlock,
        invocation: &ToolInvocation,
        operation_id: OperationId,
        state: &mut CompositionOwner,
    ) -> Result<LocalCompositionCallResponse, AgentError> {
        if call.exposed_name().as_str() == state.descriptor().execute_name.as_ref() {
            Box::pin(self.drive_local_composition_exec(
                command,
                attempt_id,
                invocation,
                operation_id,
                state,
            ))
            .await
        } else if call.exposed_name().as_str() == state.descriptor().wait_name.as_ref() {
            Box::pin(self.drive_local_composition_wait(
                command,
                attempt_id,
                invocation,
                operation_id,
                state,
            ))
            .await
        } else {
            Err(AgentError::new(
                AgentErrorKind::Internal,
                "unknown local composition driver marker",
            ))
        }
    }

    async fn drive_local_composition_exec(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        invocation: &ToolInvocation,
        operation_id: OperationId,
        state: &mut CompositionOwner,
    ) -> Result<LocalCompositionCallResponse, AgentError> {
        if state.cell().is_some() {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "local composition permits at most one active cell per root turn",
            ));
        }
        let parsed = state
            .integration()
            .parse_execute(invocation.arguments().value().as_ref().clone())
            .map_err(|error| {
                AgentError::new(
                    AgentErrorKind::InvalidInput,
                    format!("local composition exec pragma is invalid: {error}"),
                )
            })?;
        let yield_time_ms = parsed.yield_time_ms;
        let max_output_tokens = parsed.max_output_tokens;
        let source = parsed.source;
        let source_digest = Digest::sha256(
            DigestDomain::Blob,
            1,
            &[
                b"plakous-agent/local-composition-source-v1",
                source.as_bytes(),
            ],
        )
        .map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "local composition source digest failed",
            )
        })?;
        let catalog = state.descriptor().nested_tools.to_vec();
        let mut resources = if let Some(resources) = state.take_resources() {
            resources
        } else {
            let bundle = state
                .integration()
                .create_session(
                    state
                        .interpreter_observation()
                        .map_or(1, InterpreterObservation::generation),
                    state.integration().max_yield_time_ms(),
                    state.integration().max_output_tokens(),
                )
                .await
                .map_err(local_composition_prestart_error)?;
            InterpreterResources::new(bundle.session, bundle.events)
        };
        let (session, events) = resources.parts();
        state.acknowledge_launch()?;
        // The interpreter already launched; recording that settles it.
        if let Err(error) = self
            .transition_operation_as(
                StoreAdmission::Settlement,
                command.turn_id,
                operation_id,
                DurableOperationState::Dispatched,
            )
            .await
        {
            let cleanup = match session.shutdown().await {
                LocalCompositionShutdownResult::Joined { child_exit } => {
                    emit_shutdown_child_exit(
                        Some(&LocalCompositionShutdownTelemetry {
                            emitter: Arc::clone(&self.telemetry),
                            clock: Arc::clone(&self.clock),
                            context: self.telemetry_context.with_turn(command.turn_id),
                        }),
                        &LocalCompositionShutdownResult::Joined { child_exit },
                    );
                    Vec::new()
                }
                LocalCompositionShutdownResult::Lost(shutdown) => {
                    vec![local_composition_started_error(shutdown)]
                }
            };
            drop(resources);
            return Err(error_with_cleanup_disposition(error, &cleanup));
        }
        state.begin_evaluation(operation_id, invocation);
        if state.has_active_nested() || !state.deliveries_are_empty() {
            let primary = local_composition_started_error(
                "local composition retained active nested work before a new cell",
            );
            let cleanup = join_extracted_local_composition_owner(
                state,
                resources,
                false,
                Some(self.local_composition_shutdown_telemetry_for_turn(command.turn_id)),
            )
            .await;
            return Err(error_with_cleanup_disposition(primary, &cleanup));
        }
        let source_deadline = tokio::time::Instant::now() + self.config.model_operation_timeout;
        let source_authority = self.start_authority();
        let source_cancellation = command.cancellation.clone();
        let request = LocalCompositionExecuteRequest {
            start_admission: crate::local_composition::LocalCompositionStartAdmission::new(
                move || {
                    let authority = source_authority.clone();
                    let cancellation = source_cancellation.clone();
                    Box::pin(async move {
                        let _guard = authority.admit().await.map_err(|_| {
                            LocalCompositionSessionLoss::HostLost(
                                "source admission lost branch authority".into(),
                            )
                        })?;
                        if cancellation.is_cancelled()
                            || tokio::time::Instant::now() >= source_deadline
                        {
                            return Err(LocalCompositionSessionLoss::HostLost(
                                "source admission was cancelled or expired".into(),
                            ));
                        }
                        Ok(())
                    })
                },
            ),
            tool_call_id: invocation.call_id().to_string(),
            enabled_tools: catalog,
            source,
            yield_time_ms,
            max_output_tokens,
        };
        let started = async {
            let lease_guard = self.start_authority().admit().await?;
            if command.cancellation.is_cancelled() || self.shutdown.is_cancelled() {
                command.cancellation.cancel();
                return Err(cancelled_error());
            }
            let execute = session.execute(request);
            drop(lease_guard);
            tokio::pin!(execute);
            let execute_deadline = tokio::time::sleep_until(source_deadline);
            tokio::pin!(execute_deadline);
            tokio::select! {
                () = self.shutdown.cancelled() => {
                    command.cancellation.cancel();
                    Err(cancelled_error())
                }
                () = command.cancellation.cancelled() => Err(cancelled_error()),
                () = &mut execute_deadline => Err(local_composition_started_error(
                    "local composition execute exceeded its bounded operation deadline",
                )),
                result = &mut execute => result.map_err(|loss| {
                    local_composition_session_loss_error(state, &loss)
                }),
            }
        };
        let started = match started.await {
            Ok(started) => started,
            Err(error) => {
                let cleanup = join_extracted_local_composition_owner(
                    state,
                    resources,
                    false,
                    Some(self.local_composition_shutdown_telemetry_for_turn(command.turn_id)),
                )
                .await;
                return Err(error_with_cleanup_disposition(error, &cleanup));
            }
        };
        if let Err(primary) = state.acknowledge_source(started.cell_id.clone()) {
            let cleanup = join_extracted_local_composition_owner(
                state,
                resources,
                true,
                Some(self.local_composition_shutdown_telemetry_for_turn(command.turn_id)),
            )
            .await;
            return Err(error_with_cleanup_disposition(primary, &cleanup));
        }
        state.record_source_digest(source_digest);
        let response = started.initial_response;
        tokio::pin!(response);
        let mut nested_executions = FuturesUnordered::<LocalCompositionNestedFuture>::new();
        let response_deadline = tokio::time::sleep(self.config.model_operation_timeout);
        tokio::pin!(response_deadline);
        let mut steer_settlement_started = false;
        let mut nested_was_settled = true;
        let result: Result<LocalCompositionResponse, AgentError> = loop {
            let steer_pending = state.integration().runtime_contract().supports_yield
                && *self.tool_preempt.borrow() == plakous_tools::ToolPreempt::Steer;
            let executions_settled = nested_executions.is_empty()
                && !state.has_active_nested()
                && state.completed_is_empty();
            // Nested execution keeps its own budget. Once it settles, bound
            // delivery acknowledgements afresh rather than joining on an old
            // outer observation deadline that expired inside the nested call.
            if executions_settled
                && (!nested_was_settled || (steer_pending && !steer_settlement_started))
            {
                response_deadline
                    .as_mut()
                    .reset(tokio::time::Instant::now() + self.config.model_operation_timeout);
                steer_settlement_started = steer_pending;
            }
            nested_was_settled = executions_settled;
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
                    "local composition initial response exceeded its bounded operation deadline",
                )),
                response = &mut response, if state.pending_is_empty() && executions_settled && state.deliveries_are_empty() => break response.map_err(|loss| local_composition_session_loss_error(state, &loss)),
                // Never drop a nested execution: the driver retains mutation
                // settlement authority and drains an admitted read-only group.
                () = async {}, if steer_pending
                    && executions_settled && state.deliveries_are_empty() => {
                    break Ok(LocalCompositionResponse::Yielded {
                        cell_id: started.cell_id.clone(),
                        items: Vec::new(),
                    });
                }
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
        };
        let result = match result {
            Ok(result) => result,
            Err(primary) => {
                command.cancellation.cancel();
                let mut cleanup_errors = Vec::new();
                while let Some(completion) = nested_executions.next().await {
                    if let Err(error) = self
                        .buffer_local_composition_nested_read_only(command, state, completion)
                        .await
                    {
                        cleanup_errors.push(error);
                    }
                }
                fence_pending_local_composition_nested(state, "local composition cell was fenced");
                cleanup_errors.extend(
                    join_extracted_local_composition_owner(
                        state,
                        resources,
                        true,
                        Some(self.local_composition_shutdown_telemetry_for_turn(command.turn_id)),
                    )
                    .await,
                );
                return Err(error_with_cleanup_disposition(primary, &cleanup_errors));
            }
        };
        if local_composition_response_terminal(&result) {
            if let Err(primary) = state.acknowledge_terminal() {
                let cleanup = join_extracted_local_composition_owner(
                    state,
                    resources,
                    true,
                    Some(self.local_composition_shutdown_telemetry_for_turn(command.turn_id)),
                )
                .await;
                return Err(error_with_cleanup_disposition(primary, &cleanup));
            }
            self.settle_terminal_local_composition_resources(command.turn_id, state, resources)
                .await?;
        } else {
            state.restore_resources(resources);
        }
        Ok(LocalCompositionCallResponse {
            response: result,
            max_output_tokens,
            wall_time: state
                .started_at()
                .map_or(Duration::ZERO, |started| started.elapsed()),
        })
    }

    async fn drive_local_composition_wait(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        invocation: &ToolInvocation,
        operation_id: OperationId,
        state: &mut CompositionOwner,
    ) -> Result<LocalCompositionCallResponse, AgentError> {
        let input = state
            .integration()
            .parse_wait(invocation.arguments().value().as_ref().clone())
            .map_err(|error| AgentError::new(AgentErrorKind::InvalidInput, error))?;
        let max_output_tokens = input.max_output_tokens;
        if max_output_tokens > state.integration().max_output_tokens() {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "local composition wait max_tokens exceeds the host boundary",
            ));
        }
        let cell = input.cell_id;
        if state.cell() != Some(cell.as_str()) {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "local composition wait named a foreign cell",
            ));
        }
        let wait_request = if input.terminate {
            None
        } else {
            Some(LocalCompositionWaitRequest {
                cell_id: cell.clone(),
                yield_time_ms: input.yield_time_ms.min(600_000),
            })
        };
        // Waiting continues a cell that is already running.
        self.transition_operation_as(
            StoreAdmission::Settlement,
            command.turn_id,
            operation_id,
            DurableOperationState::Dispatched,
        )
        .await?;
        let mut resources = take_local_composition_owner(state)?;
        let (session, events) = resources.parts();
        let mut nested_executions = FuturesUnordered::<LocalCompositionNestedFuture>::new();
        let result = if input.terminate {
            match session.terminate(cell.clone()).await {
                LocalCompositionTerminateResult::Joined(outcome) => {
                    local_composition_wait_response(outcome)
                }
                LocalCompositionTerminateResult::Lost(error) => {
                    Err(local_composition_started_error(error))
                }
            }
        } else {
            let request = wait_request.ok_or_else(|| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "local composition wait request is absent",
                )
            })?;
            let observation_timeout =
                Duration::from_millis(request.yield_time_ms) + Duration::from_secs(5);
            let response = session.wait(request);
            tokio::pin!(response);
            let response_deadline = tokio::time::sleep(observation_timeout);
            tokio::pin!(response_deadline);
            let mut steer_settlement_started = false;
            let mut nested_was_settled = true;
            loop {
                let steer_pending = state.integration().runtime_contract().supports_yield
                    && *self.tool_preempt.borrow() == plakous_tools::ToolPreempt::Steer;
                let executions_settled = nested_executions.is_empty()
                    && !state.has_active_nested()
                    && state.completed_is_empty();
                if executions_settled
                    && (!nested_was_settled || (steer_pending && !steer_settlement_started))
                {
                    let settlement_timeout = if steer_pending {
                        self.config.model_operation_timeout
                    } else {
                        observation_timeout
                    };
                    response_deadline
                        .as_mut()
                        .reset(tokio::time::Instant::now() + settlement_timeout);
                    steer_settlement_started = steer_pending;
                }
                nested_was_settled = executions_settled;
                tokio::select! {
                    biased;
                    () = self.shutdown.cancelled() => {
                        command.cancellation.cancel();
                        break Err(cancelled_error());
                    }
                    () = command.cancellation.cancelled() => break Err(cancelled_error()),
                    control = self.control_receiver.recv() => {
                        let Some(control) = control else {
                            command.cancellation.cancel();
                            break Err(cancelled_error());
                        };
                        Box::pin(self.handle_control(Some(command), control)).await;
                    }
                    () = &mut response_deadline, if executions_settled && (!steer_pending || steer_settlement_started) => break Err(local_composition_started_error(
                        "local composition wait exceeded its bounded operation deadline",
                    )),
                    response = &mut response, if state.pending_is_empty() && nested_executions.is_empty() && state.completed_is_empty() && state.deliveries_are_empty() => {
                        break match response {
                            Err(error) => Err(local_composition_started_error(error)),
                            Ok(LocalCompositionWaitOutcome::Lost(loss)) => {
                                match observe_local_composition_session_loss(state, &loss) {
                                    Ok(()) => Err(local_composition_started_error(loss)),
                                    Err(error) => Err(error),
                                }
                            }
                            Ok(outcome) => local_composition_wait_response(outcome),
                        };
                    }
                    () = async {}, if steer_pending
                        && executions_settled && state.deliveries_are_empty() => {
                        break Ok(LocalCompositionResponse::Yielded {
                            cell_id: cell.clone(),
                            items: Vec::new(),
                        });
                    }
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
        };
        let result = match result {
            Ok(result) => result,
            Err(primary) => {
                command.cancellation.cancel();
                let mut cleanup_errors = Vec::new();
                while let Some(completion) = nested_executions.next().await {
                    if let Err(error) = self
                        .buffer_local_composition_nested_read_only(command, state, completion)
                        .await
                    {
                        cleanup_errors.push(error);
                    }
                }
                fence_pending_local_composition_nested(state, "local composition wait was fenced");
                cleanup_errors.extend(
                    join_extracted_local_composition_owner(
                        state,
                        resources,
                        true,
                        Some(self.local_composition_shutdown_telemetry_for_turn(command.turn_id)),
                    )
                    .await,
                );
                return Err(error_with_cleanup_disposition(primary, &cleanup_errors));
            }
        };
        if matches!(&result, LocalCompositionResponse::Terminated { .. }) && input.terminate {
            if let Err(primary) = state.acknowledge_termination() {
                let cleanup = join_extracted_local_composition_owner(
                    state,
                    resources,
                    false,
                    Some(self.local_composition_shutdown_telemetry_for_turn(command.turn_id)),
                )
                .await;
                return Err(error_with_cleanup_disposition(primary, &cleanup));
            }
            let cleanup = join_extracted_local_composition_owner(
                state,
                resources,
                false,
                Some(self.local_composition_shutdown_telemetry_for_turn(command.turn_id)),
            )
            .await;
            if let Some((first, remaining)) = cleanup.split_first() {
                return Err(error_with_cleanup_disposition(first.clone(), remaining));
            }
            state.retire_cell();
        } else if local_composition_response_terminal(&result) {
            if let Err(primary) = state.acknowledge_terminal() {
                let cleanup = join_extracted_local_composition_owner(
                    state,
                    resources,
                    true,
                    Some(self.local_composition_shutdown_telemetry_for_turn(command.turn_id)),
                )
                .await;
                return Err(error_with_cleanup_disposition(primary, &cleanup));
            }
            self.settle_terminal_local_composition_resources(command.turn_id, state, resources)
                .await?;
        } else {
            state.restore_resources(resources);
        }
        Ok(LocalCompositionCallResponse {
            response: result,
            max_output_tokens,
            wall_time: state
                .started_at()
                .map_or(Duration::ZERO, |started| started.elapsed()),
        })
    }

    /// Retains or retires the evaluator behind a cell that just ended.
    ///
    /// A healthy generation is restored for the next cell. A generation that
    /// died with its own definite outcome is joined here instead, under a typed
    /// reset: the cell's result is already known, so it is delivered rather
    /// than turned into recovery, and the next cell starts a fresh generation
    /// without re-evaluating any source.
    async fn settle_terminal_local_composition_resources(
        &self,
        turn_id: TurnId,
        state: &mut CompositionOwner,
        resources: InterpreterResources,
    ) -> Result<(), AgentError> {
        if !resources.generation_lost() {
            state.restore_terminal_resources(resources);
            return Ok(());
        }
        state.retire_cell();
        let mut cleanup = Vec::new();
        if let Err(observation) = observe_local_composition_session_loss(
            state,
            &LocalCompositionSessionLoss::ProtocolViolation(
                "local composition evaluator generation ended with its cell".to_owned(),
            ),
        ) {
            cleanup.push(observation);
        }
        cleanup.extend(
            join_extracted_local_composition_owner(
                state,
                resources,
                false,
                Some(self.local_composition_shutdown_telemetry_for_turn(turn_id)),
            )
            .await,
        );
        match cleanup.split_first() {
            Some((primary, remaining)) => {
                Err(error_with_cleanup_disposition(primary.clone(), remaining))
            }
            None => Ok(()),
        }
    }

    fn handle_local_composition_broker_event(
        &mut self,
        state: &mut CompositionOwner,
        event: Option<LocalCompositionEvent>,
    ) -> Result<(), AgentError> {
        let Some(event) = event else {
            return Err(local_composition_started_error(
                "local composition broker closed while a cell was active",
            ));
        };
        match event {
            LocalCompositionEvent::Notification {
                notification,
                responder,
            } => {
                // Nothing a notification can be wrong about has an effect, so
                // every definite refusal is answered to the notifier and the
                // turn carries on. Only an error the driver itself cannot
                // decide still propagates.
                let (call_id, correlation) = match state
                    .notification_correlation(&notification.cell_id, &notification.call_id)
                {
                    Ok(correlation) => correlation,
                    Err(error) if local_composition_recovery_authoritative(&error) => {
                        return Err(error);
                    }
                    Err(error) => {
                        responder.reject(error.message().to_owned());
                        return Ok(());
                    }
                };
                let text = clip_local_composition_update_text(
                    &notification.text,
                    &mut self.observation_budget,
                )?;
                let Ok(update) = ToolResultUpdateBlock::new(
                    call_id,
                    ToolExecutionOwner::Local,
                    text,
                    correlation,
                ) else {
                    responder.reject("local composition notification is not canonical".to_owned());
                    return Ok(());
                };
                state.push_update(update);
                if !responder.acknowledge() {
                    return Err(local_composition_started_error(
                        "local composition notification acknowledgement receiver disappeared",
                    ));
                }
            }
            LocalCompositionEvent::NestedCall { call, responder } => {
                state.admit_nested(call, responder)?;
            }
        }
        Ok(())
    }

    /// Commits inside a running cell are settlement, so a nested call is the
    /// cell's new work and meets the ordinary threshold here, before anything
    /// is journaled. The refusal is a definite error the cell can catch.
    async fn refuse_nested_under_store_pressure(&self) -> Result<(), AgentError> {
        match self.authority.ordinary_store_pressure().await {
            None => Ok(()),
            Some(capacity) => Err(AgentError::new(
                AgentErrorKind::Backpressure,
                format!(
                    "nested call refused under storage pressure: {}",
                    SessionStoreError::Capacity(capacity)
                ),
            )),
        }
    }

    async fn dispatch_next_local_composition_nested(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        state: &mut CompositionOwner,
    ) -> Result<(), AgentError> {
        let Some(pending) = state.take_serial_pending()? else {
            return Ok(());
        };
        let runtime_call_id = pending.call.runtime_call_id.clone();
        self.nested_execution_value = None;
        let result = Box::pin(self.execute_local_composition_nested(
            command,
            attempt_id,
            state,
            pending.call,
            pending.request_digest,
            pending.request_sequence,
        ))
        .await;
        let execution = self
            .nested_execution_value
            .take()
            .filter(|_| result.is_ok());
        state.finish_serial(&runtime_call_id, &result)?;
        let waiters = take_local_composition_nested_waiters(state, &runtime_call_id);
        queue_local_composition_nested_results(state, pending.responder, waiters, result, execution)
    }

    #[allow(
        clippy::result_large_err,
        reason = "the refusal hands the pending nested call back so the caller can settle it verbatim"
    )]
    async fn start_local_composition_nested_read_only(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        state: &mut CompositionOwner,
        pending: PendingLocalCompositionNested,
    ) -> Result<LocalCompositionNestedFuture, (PendingLocalCompositionNested, AgentError)> {
        const MAX_LOCAL_COMPOSITION_DEPTH: u16 = 1;
        let nested = &pending.call;
        let prepared = async {
            if state.cell() != Some(nested.cell_id.as_str()) {
                return Err(AgentError::new(
                    AgentErrorKind::InvalidInput,
                    "nested local composition call belongs to a foreign cell",
                ));
            }
            let evaluation_operation = state.evaluation_operation().ok_or_else(|| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "nested local composition call has no owning evaluation operation",
                )
            })?;
            let interpreter_generation = state
                .interpreter_observation()
                .map_or(1, InterpreterObservation::generation);
            let canonical_request = serde_json::to_vec(&(
                "plakous-local-composition-child-request-v1",
                nested.cell_id.as_str(),
                nested.runtime_call_id.as_str(),
                &nested.tool_name,
                &nested.input,
            ))
            .map_err(|_| AgentError::new(AgentErrorKind::InvalidInput, "nested local composition request could not be canonicalized"))?;
            // Nothing is journaled or executed until `prepare_operation`
            // below, so a failure to canonicalize or bind the edge is a
            // definite refusal of a call that never ran. It reaches the cell as
            // an ordinary error it can catch, not as a recovery boundary.
            let edge_request = serde_json::to_vec(&(
                "plakous-local-composition-child-edge-v2",
                evaluation_operation.0,
                interpreter_generation,
                nested.cell_id.as_str(),
                pending.request_sequence,
                pending.request_digest,
                &canonical_request,
            ))
            .map_err(|_| AgentError::new(AgentErrorKind::InvalidInput, "nested local composition edge could not be canonicalized"))?;
            let edge_binding = Digest::sha256(
                DigestDomain::ProviderOperation,
                4,
                &[b"plakous-agent/local-composition-child-edge-v2", &edge_request],
            )
            .map_err(|_| AgentError::new(AgentErrorKind::Identity, "nested local composition edge identity could not be constructed"))?;
            let call_id = local_composition_child_call_id(edge_binding)?;
            let tool_name = plakous_model::ToolName::new(nested.tool_name.clone())
                .map_err(|_| AgentError::new(AgentErrorKind::InvalidInput, "nested tool name is invalid"))?;
            if tool_name.as_str() == state.descriptor().execute_name.as_ref()
                || tool_name.as_str() == state.descriptor().wait_name.as_ref()
            {
                return Err(AgentError::new(
                    AgentErrorKind::InvalidInput,
                    format!("local composition exceeded its independent recursion depth of {MAX_LOCAL_COMPOSITION_DEPTH}"),
                ));
            }
            let owner = local_composition_nested_execution_owner(
                &self.tools,
                &state.descriptor().nested_projection,
                &tool_name,
            )?;
            let raw = local_composition_nested_call_arguments(
                &state.descriptor().nested_projection,
                &tool_name,
                nested.input.as_ref(),
            )?;
            self.refuse_nested_under_store_pressure().await?;
            let edge_operation = self
                .prepare_operation(
                    StoreAdmission::Settlement,
                    command.turn_id,
                    DurableOperationKind::CompositionChild {
                        turn_id: command.turn_id,
                        attempt_id,
                        call_id,
                        evaluation_operation_id: evaluation_operation.0,
                        interpreter_generation,
                        cell_id: nested.cell_id.clone(),
                        request_sequence: pending.request_sequence,
                    },
                    edge_binding,
                    Some(edge_request),
                    None,
                )
                .await
                .map_err(|error| local_composition_started_error(error.message()))?;
            let call = ToolCallBlock::new(
                call_id,
                tool_name,
                owner,
                ToolCallState::Requested,
                ToolCorrelation::new(
                    Some(nested.runtime_call_id.clone()),
                    None,
                    plakous_model::ToolCallerLineage::local_programmatic(
                        evaluation_operation.0,
                        interpreter_generation,
                        nested.cell_id.clone(),
                        pending.request_sequence,
                        call_id,
                        MAX_LOCAL_COMPOSITION_DEPTH,
                    )
                    .map_err(|_| AgentError::new(AgentErrorKind::InvalidInput, "nested local composition caller lineage is invalid"))?,
                )
                .map_err(|_| AgentError::new(AgentErrorKind::InvalidInput, "nested correlation is invalid"))?,
                raw,
            )
            .map_err(|_| AgentError::new(AgentErrorKind::InvalidInput, "nested tool call is invalid"))?;
            let presentation = ToolPresentation::V2(Arc::new(state.descriptor().nested_projection.clone()));
            let canonical = self.canonical_tool_name(command, attempt_id, &presentation, &call);
            let call = call.with_canonical_name(canonical);
            let admission = match self
                .admit_tool_with_presentation(
                    command,
                    attempt_id,
                    &call,
                    presentation,
                    ToolDispatchOrigin::NestedComposition,
                    state.exec_call_id(),
                )
                .await
            {
                Ok(admission) => admission,
                Err(error) => {
                    let evidence = runtime_component(
                        RuntimeComponentKind::Operation,
                        serde_json::to_vec(&(
                            "plakous-local-composition-child-admission-failure-v1",
                            evaluation_operation.0,
                            pending.request_sequence,
                            edge_binding,
                            error.kind(),
                        ))
                        .map_err(|_| {
                            AgentError::new(
                                AgentErrorKind::Internal,
                                "nested admission failure evidence could not be encoded",
                            )
                        })?,
                    )?;
                    self.transition_operation(
                        command.turn_id,
                        edge_operation,
                        DurableOperationState::ProvenNotDispatched(evidence),
                    )
                    .await?;
                    state.record_child_operation(edge_operation);
                    return Err(error);
                }
            };
            let (execution, audit_identity) = match admission {
                ToolAdmission::Admitted { execution, audit_identity } => {
                    (execution, audit_identity)
                }
                ToolAdmission::RejectedBeforeValidation { execution, .. }
                | ToolAdmission::RejectedAfterValidation { execution } => {
                    let rejected_operation = self
                        .prepare_rejected_tool_operation(
                            command.turn_id,
                            attempt_id,
                            0,
                            &call,
                            self.entry_id()?,
                            &execution,
                        )
                        .await?;
                    let evidence = runtime_component(
                        RuntimeComponentKind::Operation,
                        serde_json::to_vec(&(
                            "plakous-local-composition-child-rejected-v1",
                            evaluation_operation.0,
                            pending.request_sequence,
                            edge_binding,
                        ))
                        .map_err(|_| {
                            AgentError::new(
                                AgentErrorKind::Internal,
                                "nested rejection evidence could not be encoded",
                            )
                        })?,
                    )?;
                    self.transition_operation(
                        command.turn_id,
                        edge_operation,
                        DurableOperationState::ProvenNotDispatched(evidence),
                    )
                    .await?;
                    state.record_child_operation(rejected_operation);
                    state.record_child_operation(edge_operation);
                    return Err(AgentError::new(
                        AgentErrorKind::InvalidInput,
                        "read-only concurrent nested call was rejected before dispatch",
                    ));
                }
                ToolAdmission::DecisionRequired {
                    pending: decision,
                    ..
                } => {
                    drop(decision);
                    let error = local_composition_nested_decision_unsupported();
                    let evidence = runtime_component(
                        RuntimeComponentKind::Operation,
                        serde_json::to_vec(&(
                            "plakous-local-composition-child-unsupported-v1",
                            evaluation_operation.0,
                            pending.request_sequence,
                            edge_binding,
                        ))
                        .map_err(|_| {
                            AgentError::new(
                                AgentErrorKind::Internal,
                                "nested unsupported-capability evidence could not be encoded",
                            )
                        })?,
                    )?;
                    self.transition_operation(
                        command.turn_id,
                        edge_operation,
                        DurableOperationState::ProvenNotDispatched(evidence),
                    )
                    .await?;
                    state.record_child_operation(edge_operation);
                    return Err(AgentError::from_tool(&error));
                }
            };
            let AdmittedToolExecution::ReadOnly {
                invocation,
                projection_receipt,
                authorized,
                effect,
            } = *execution else {
                return Err(AgentError::new(
                    AgentErrorKind::Internal,
                    "non-read-only tool entered the concurrent nested scheduler",
                ));
            };
            if effect != ToolEffect::ReadOnly {
                return Err(AgentError::new(AgentErrorKind::Internal, "nested read-only effect drifted"));
            }
            let binding = canonical_tool_operation_binding(0, &invocation)?;
            let input = serde_json::to_vec(&(binding, &call)).map_err(|_| AgentError::new(AgentErrorKind::Internal, "nested read-only operation input could not be captured"))?;
            let tool_operation = self
                .prepare_operation(
                    StoreAdmission::Settlement,
                    command.turn_id,
                    DurableOperationKind::Effect {
                        turn_id: command.turn_id,
                        attempt_id,
                        call_id,
                        source_index: 0,
                    },
                    binding,
                    Some(input),
                    None,
                )
                .await?;
            self.transition_operations_as(
                StoreAdmission::Settlement,
                command.turn_id,
                vec![
                    (edge_operation, DurableOperationState::Dispatched),
                    (tool_operation, DurableOperationState::Dispatched),
                ],
            )
            .await?;
            let result_entry_id = self.entry_id()?;
            // The nested scheduler borrows the driver out of `self`, so the
            // owning marker call is read from the borrowed state.
            let lineage = ToolCallerLineageView::from_lineage(
                call.correlation().caller_lineage(),
                state.exec_call_id(),
            );
            let dispatch_witness = Arc::new(AtomicBool::new(false));
            let process_session_update = Arc::new(Mutex::new(None));
            let request = IndexedReadOnlyRequest {
                authority: self.start_authority(),
                operation_id: tool_operation,
                tools: self.tools.clone(),
                tool_presentation: ToolPresentation::V2(Arc::new(state.descriptor().nested_projection.clone())),
                invocation: *invocation,
                projection_receipt,
                authorized: *authorized,
                audit_identity,
                lineage,
                index: 0,
                call,
                result_entry_id,
                cancellation: command.cancellation.clone(),
                shutdown: self.shutdown.clone(),
                preempt: tokio::sync::watch::channel(plakous_tools::ToolPreempt::None).1,
                preempt_guard: None,
                clock: Arc::clone(&self.clock),
                progress_capacity: self.config.tool_progress_capacity,
                events: command.shared.events.clone(),
                turn_id: command.turn_id,
                attempt_id,
                telemetry: Arc::clone(&self.telemetry),
                telemetry_context: self.telemetry_context.with_turn(command.turn_id).with_attempt(attempt_id).with_call(call_id),
                turn_shared: Arc::clone(&command.shared),
                dispatch_witness,
                process_session_update,
            };
            Ok((evaluation_operation, edge_operation, edge_binding, request))
        }
        .await;
        let events = command.shared.events.clone();
        let turn_id = command.turn_id;
        match prepared {
            Ok((evaluation_operation, edge_operation, edge_binding, request)) => {
                Ok(Box::pin(async move {
                    let execution = execute_indexed_read_only(request).await;
                    // A nested read-only child never enters the group scheduler,
                    // so it must publish the same terminal observation the group
                    // path publishes. Without it a composition cell has started
                    // children and no terminal states, and every consumer that
                    // tallies a cell (batch counts) undercounts.
                    if execution.result.is_ok() {
                        let _ = events.publish(
                            EventContext::call(turn_id, attempt_id, execution.call.call_id()),
                            AgentEventKind::ToolCompleted {
                                identity: execution.audit_identity.clone(),
                                lineage: execution.lineage.clone(),
                                observation: execution.result.as_ref().ok().map(|result| {
                                    tool_result_observation_with_subject(
                                        &result.result,
                                        result.presentation_subject.as_deref(),
                                    )
                                }),
                            },
                        );
                    }
                    LocalCompositionNestedReadOnlyCompletion {
                        pending,
                        edge_operation,
                        edge_binding,
                        evaluation_operation,
                        execution,
                    }
                }))
            }
            Err(error) => Err((pending, error)),
        }
    }

    async fn buffer_local_composition_nested_read_only(
        &mut self,
        command: &PromptCommand,
        state: &mut CompositionOwner,
        completion: LocalCompositionNestedReadOnlyCompletion,
    ) -> Result<(), AgentError> {
        state.buffer_read_only(completion)?;
        self.drain_local_composition_nested(command, state).await
    }

    async fn buffer_local_composition_nested_rejection(
        &mut self,
        command: &PromptCommand,
        state: &mut CompositionOwner,
        pending: PendingLocalCompositionNested,
        error: AgentError,
    ) -> Result<(), AgentError> {
        state.buffer_rejection(pending, error)?;
        self.drain_local_composition_nested(command, state).await
    }

    async fn drain_local_composition_nested(
        &mut self,
        command: &PromptCommand,
        state: &mut CompositionOwner,
    ) -> Result<(), AgentError> {
        while let Some(ready) = state.take_next_ready()? {
            match ready {
                LocalCompositionNestedReady::Execution(completion) => {
                    self.commit_local_composition_nested_read_only(command, state, *completion)
                        .await?;
                }
                LocalCompositionNestedReady::Rejected(rejected) => {
                    let (pending, error) = *rejected;
                    state.record_nested_disposition(
                        &pending.call.runtime_call_id,
                        if local_composition_recovery_authoritative(&error) {
                            LocalCompositionNestedDisposition::RecoveryRequired
                        } else {
                            LocalCompositionNestedDisposition::Rejected(error.message().to_owned())
                        },
                    );
                    let waiters =
                        take_local_composition_nested_waiters(state, &pending.call.runtime_call_id);
                    queue_local_composition_nested_results(
                        state,
                        pending.responder,
                        waiters,
                        Err(error),
                        None,
                    )?;
                }
            }
        }
        Ok(())
    }

    async fn commit_local_composition_nested_read_only(
        &mut self,
        command: &PromptCommand,
        state: &mut CompositionOwner,
        completion: LocalCompositionNestedReadOnlyCompletion,
    ) -> Result<(), AgentError> {
        let LocalCompositionNestedReadOnlyCompletion {
            pending,
            edge_operation,
            edge_binding,
            evaluation_operation,
            execution,
        } = completion;
        if let Some(update) = execution.process_session_update {
            self.apply_process_session_update(command.turn_id, update)
                .await?;
        }
        let terminal = tool_operation_terminal_state(
            &execution.result,
            execution.dispatched,
            execution.result_entry_id,
        )?;
        self.transition_operation(command.turn_id, execution.operation_id, terminal)
            .await?;
        let mut execution_value = None;
        let result = match execution.result {
            Ok(mut result) => {
                execution_value = result.execution_value.take();
                // A nested composition child's own result goes to the cell,
                // not the model, so the observation budget passes it through
                // unclipped; the composed answer is clipped and announced once
                // at the outer site.
                let (item, _) = canonicalize_observed_tool_execution(
                    execution.result_entry_id,
                    result,
                    &mut self.observation_budget,
                )?;
                self.record_nested_committed_observation(&item);
                local_composition_nested_result_value(&item)
            }
            Err(error) => Err(error),
        };
        let recovery = result
            .as_ref()
            .err()
            .is_some_and(local_composition_recovery_authoritative);
        let evidence = runtime_component(
            RuntimeComponentKind::Operation,
            serde_json::to_vec(&(
                "plakous-local-composition-child-completed-v2",
                evaluation_operation.0,
                pending.request_sequence,
                edge_binding,
                execution.operation_id.0,
                result.is_ok(),
            ))
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "nested completion evidence could not be encoded",
                )
            })?,
        )?;
        self.transition_operation(
            command.turn_id,
            edge_operation,
            if recovery {
                DurableOperationState::OutcomeUnknown(evidence)
            } else {
                DurableOperationState::Completed(evidence)
            },
        )
        .await?;
        state.record_child_operation(execution.operation_id);
        state.record_child_operation(edge_operation);
        state.record_nested_disposition(
            &pending.call.runtime_call_id,
            match &result {
                Ok(value) => LocalCompositionNestedDisposition::Resolved(value.clone()),
                Err(error) if local_composition_recovery_authoritative(error) => {
                    LocalCompositionNestedDisposition::RecoveryRequired
                }
                Err(error) => {
                    LocalCompositionNestedDisposition::Rejected(error.message().to_owned())
                }
            },
        );
        let waiters = take_local_composition_nested_waiters(state, &pending.call.runtime_call_id);
        let execution_value = execution_value.filter(|_| result.is_ok());
        queue_local_composition_nested_results(
            state,
            pending.responder,
            waiters,
            result,
            execution_value,
        )
    }

    async fn execute_local_composition_nested(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        state: &mut CompositionOwner,
        nested: crate::local_composition::LocalCompositionNestedCall,
        request_digest: Digest,
        request_sequence: u32,
    ) -> Result<serde_json::Value, AgentError> {
        const MAX_LOCAL_COMPOSITION_DEPTH: u16 = 1;
        if state.cell() != Some(nested.cell_id.as_str()) {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "nested local composition call belongs to a foreign cell",
            ));
        }
        let evaluation_operation = state.evaluation_operation().ok_or_else(|| {
            AgentError::new(
                AgentErrorKind::Internal,
                "nested local composition call has no owning evaluation operation",
            )
        })?;
        let interpreter_generation = state
            .interpreter_observation()
            .map_or(1, InterpreterObservation::generation);
        let canonical_request = serde_json::to_vec(&(
            "plakous-local-composition-child-request-v1",
            nested.cell_id.as_str(),
            nested.runtime_call_id.as_str(),
            &nested.tool_name,
            &nested.input,
        ))
        .map_err(|_| {
            AgentError::new(
                AgentErrorKind::InvalidInput,
                "nested local composition request could not be canonicalized",
            )
        })?;
        let runtime_tool_call_id = nested.runtime_call_id.clone();
        // Pre-dispatch, exactly as in the read-only path above: no durable
        // operation exists yet and no tool has run, so these are definite
        // refusals rather than unknown outcomes.
        let edge_request = serde_json::to_vec(&(
            "plakous-local-composition-child-edge-v2",
            evaluation_operation.0,
            interpreter_generation,
            nested.cell_id.as_str(),
            request_sequence,
            request_digest,
            &canonical_request,
        ))
        .map_err(|_| {
            AgentError::new(
                AgentErrorKind::InvalidInput,
                "nested local composition edge could not be canonicalized",
            )
        })?;
        let edge_binding = Digest::sha256(
            DigestDomain::ProviderOperation,
            4,
            &[
                b"plakous-agent/local-composition-child-edge-v2",
                &edge_request,
            ],
        )
        .map_err(|_| {
            AgentError::new(
                AgentErrorKind::Identity,
                "nested local composition edge identity could not be constructed",
            )
        })?;
        let call_id = local_composition_child_call_id(edge_binding)?;
        let tool_name = plakous_model::ToolName::new(nested.tool_name).map_err(|_| {
            AgentError::new(AgentErrorKind::InvalidInput, "nested tool name is invalid")
        })?;
        if tool_name.as_str() == state.descriptor().execute_name.as_ref()
            || tool_name.as_str() == state.descriptor().wait_name.as_ref()
        {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                format!(
                    "local composition exceeded its independent recursion depth of {MAX_LOCAL_COMPOSITION_DEPTH}"
                ),
            ));
        }
        let owner = local_composition_nested_execution_owner(
            &self.tools,
            &state.descriptor().nested_projection,
            &tool_name,
        )?;
        let raw = local_composition_nested_call_arguments(
            &state.descriptor().nested_projection,
            &tool_name,
            nested.input.as_ref(),
        )?;
        self.refuse_nested_under_store_pressure().await?;
        let edge_operation = self
            .prepare_operation(
                StoreAdmission::Settlement,
                command.turn_id,
                DurableOperationKind::CompositionChild {
                    turn_id: command.turn_id,
                    attempt_id,
                    call_id,
                    evaluation_operation_id: evaluation_operation.0,
                    interpreter_generation,
                    cell_id: nested.cell_id.clone(),
                    request_sequence,
                },
                edge_binding,
                Some(edge_request),
                None,
            )
            .await
            .map_err(|error| local_composition_started_error(error.message()))?;
        let call = ToolCallBlock::new(
            call_id,
            tool_name,
            owner,
            ToolCallState::Requested,
            ToolCorrelation::new(
                Some(runtime_tool_call_id.clone()),
                None,
                plakous_model::ToolCallerLineage::local_programmatic(
                    evaluation_operation.0,
                    interpreter_generation,
                    nested.cell_id.clone(),
                    request_sequence,
                    call_id,
                    MAX_LOCAL_COMPOSITION_DEPTH,
                )
                .map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::InvalidInput,
                        "nested local composition caller lineage is invalid",
                    )
                })?,
            )
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::InvalidInput,
                    "nested correlation is invalid",
                )
            })?,
            raw,
        )
        .map_err(|_| {
            AgentError::new(AgentErrorKind::InvalidInput, "nested tool call is invalid")
        })?;
        let presentation =
            ToolPresentation::V2(Arc::new(state.descriptor().nested_projection.clone()));
        let canonical = self.canonical_tool_name(command, attempt_id, &presentation, &call);
        let call = call.with_canonical_name(canonical);
        let admission = match self
            .admit_tool_with_presentation(
                command,
                attempt_id,
                &call,
                presentation,
                ToolDispatchOrigin::NestedComposition,
                state.exec_call_id(),
            )
            .await
        {
            Ok(admission) => admission,
            Err(error) => {
                let evidence = runtime_component(
                    RuntimeComponentKind::Operation,
                    serde_json::to_vec(&(
                        "plakous-local-composition-child-admission-failure-v1",
                        evaluation_operation.0,
                        request_sequence,
                        edge_binding,
                        error.kind(),
                    ))
                    .map_err(|_| {
                        AgentError::new(
                            AgentErrorKind::Internal,
                            "nested admission failure evidence could not be encoded",
                        )
                    })?,
                )?;
                self.transition_operation(
                    command.turn_id,
                    edge_operation,
                    DurableOperationState::ProvenNotDispatched(evidence),
                )
                .await?;
                state.record_child_operation(edge_operation);
                state.record_nested_disposition(
                    &runtime_tool_call_id,
                    LocalCompositionNestedDisposition::Rejected(error.message().to_owned()),
                );
                return Err(error);
            }
        };
        let result_entry_id = self.entry_id()?;
        let mut items = vec![None];
        let mut operations = Vec::new();
        let mut dispatched = 0;
        let mut edge_dispatched = false;
        match admission {
            ToolAdmission::Admitted { execution, .. } => {
                self.transition_operation_as(
                    StoreAdmission::Settlement,
                    command.turn_id,
                    edge_operation,
                    DurableOperationState::Dispatched,
                )
                .await
                .map_err(|error| local_composition_started_error(error.message()))?;
                edge_dispatched = true;
                let nested_presentation =
                    ToolPresentation::V2(Arc::new(state.descriptor().nested_projection.clone()));
                let outer_presentation =
                    std::mem::replace(&mut self.tool_presentation, nested_presentation);
                let parent_call_id = state.exec_call_id();
                let execution_result = Box::pin(self.execute_restored_admitted(
                    command,
                    attempt_id,
                    0,
                    &call,
                    result_entry_id,
                    *execution,
                    &mut items,
                    &mut operations,
                    &mut dispatched,
                    ToolDispatchOrigin::NestedComposition,
                    parent_call_id,
                ))
                .await;
                self.tool_presentation = outer_presentation;
                if let Err(error) = execution_result {
                    let evidence = runtime_component(
                        RuntimeComponentKind::Operation,
                        serde_json::to_vec(&(
                            "plakous-local-composition-child-failure-v1",
                            evaluation_operation.0,
                            request_sequence,
                            edge_binding,
                            error.kind(),
                        ))
                        .map_err(|_| {
                            AgentError::new(
                                AgentErrorKind::Internal,
                                "nested local composition failure evidence could not be encoded",
                            )
                        })?,
                    )?;
                    if local_composition_recovery_authoritative(&error) {
                        state.record_nested_disposition(
                            &runtime_tool_call_id,
                            LocalCompositionNestedDisposition::RecoveryRequired,
                        );
                        self.transition_operation(
                            command.turn_id,
                            edge_operation,
                            DurableOperationState::OutcomeUnknown(evidence),
                        )
                        .await?;
                        state.record_child_operations(operations);
                        state.record_child_operation(edge_operation);
                    } else {
                        state.record_nested_disposition(
                            &runtime_tool_call_id,
                            LocalCompositionNestedDisposition::Rejected(error.message().to_owned()),
                        );
                        self.transition_operation(
                            command.turn_id,
                            edge_operation,
                            DurableOperationState::Completed(evidence),
                        )
                        .await?;
                        state.record_child_operations(operations);
                        state.record_child_operation(edge_operation);
                    }
                    return Err(error);
                }
            }
            ToolAdmission::RejectedBeforeValidation { execution, .. }
            | ToolAdmission::RejectedAfterValidation { execution } => {
                let operation = self
                    .prepare_rejected_tool_operation(
                        command.turn_id,
                        attempt_id,
                        0,
                        &call,
                        result_entry_id,
                        &execution,
                    )
                    .await?;
                let (item, clip) = canonicalize_observed_tool_execution(
                    result_entry_id,
                    execution,
                    &mut self.observation_budget,
                )?;
                publish_observation_clip(
                    &command.shared.events,
                    command.turn_id,
                    attempt_id,
                    call.call_id(),
                    call.exposed_name(),
                    clip,
                );
                items[0] = Some(item);
                operations.push(operation);
            }
            ToolAdmission::DecisionRequired { pending, .. } => {
                drop(pending);
                let error = local_composition_nested_decision_unsupported();
                let evidence = runtime_component(
                    RuntimeComponentKind::Operation,
                    serde_json::to_vec(&(
                        "plakous-local-composition-child-unsupported-v1",
                        evaluation_operation.0,
                        request_sequence,
                        edge_binding,
                    ))
                    .map_err(|_| {
                        AgentError::new(
                            AgentErrorKind::Internal,
                            "nested unsupported-capability evidence could not be encoded",
                        )
                    })?,
                )?;
                self.transition_operation(
                    command.turn_id,
                    edge_operation,
                    DurableOperationState::ProvenNotDispatched(evidence),
                )
                .await?;
                state.record_child_operation(edge_operation);
                state.record_nested_disposition(
                    &runtime_tool_call_id,
                    LocalCompositionNestedDisposition::Rejected(error.message().to_string()),
                );
                return Err(AgentError::from_tool(&error));
            }
        }
        let item = items[0].as_ref().ok_or_else(|| {
            AgentError::new(AgentErrorKind::Internal, "nested tool result is absent")
        })?;
        self.record_nested_committed_observation(item);
        let value = local_composition_nested_result_value(item);
        let evidence = runtime_component(
            RuntimeComponentKind::Operation,
            serde_json::to_vec(&(
                "plakous-local-composition-child-completed-v1",
                evaluation_operation.0,
                request_sequence,
                edge_binding,
                operations
                    .iter()
                    .map(|operation| operation.0)
                    .collect::<Vec<_>>(),
            ))
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "nested local composition completion evidence could not be encoded",
                )
            })?,
        )?;
        self.transition_operation(
            command.turn_id,
            edge_operation,
            if edge_dispatched {
                DurableOperationState::Completed(evidence)
            } else {
                DurableOperationState::ProvenNotDispatched(evidence)
            },
        )
        .await?;
        state.record_child_operations(operations);
        state.record_child_operation(edge_operation);
        state.record_nested_disposition(
            &runtime_tool_call_id,
            match &value {
                Ok(value) => LocalCompositionNestedDisposition::Resolved(value.clone()),
                Err(error) if local_composition_recovery_authoritative(error) => {
                    LocalCompositionNestedDisposition::RecoveryRequired
                }
                Err(error) => {
                    LocalCompositionNestedDisposition::Rejected(error.message().to_owned())
                }
            },
        );
        value
    }
}

#[cfg(test)]
mod failure_evidence_tests {
    use super::LocalCompositionFailureEvidence;
    use crate::AgentErrorKind;

    #[test]
    fn failure_evidence_keeps_its_tagged_tuple_bytes() -> Result<(), serde_json::Error> {
        let evidence = LocalCompositionFailureEvidence::new(
            AgentErrorKind::Tool,
            "the cell failed",
            vec![7, 9],
        );
        let bytes = serde_json::to_vec(&evidence)?;
        assert_eq!(
            bytes,
            serde_json::to_vec(&(
                "plakous-local-composition-failure-v1",
                AgentErrorKind::Tool,
                "the cell failed",
                vec![7_u64, 9],
            ))?
        );
        let decoded = LocalCompositionFailureEvidence::decode(&bytes).ok_or_else(|| {
            <serde_json::Error as serde::de::Error>::custom("failure evidence did not decode")
        })?;
        assert_eq!(decoded.detail(), "the cell failed");
        assert_eq!(decoded.child_operations(), [7, 9]);
        let other = serde_json::to_vec(&(
            "plakous-local-composition-cell-v2",
            AgentErrorKind::Tool,
            "x",
            Vec::<u64>::new(),
        ))?;
        assert!(LocalCompositionFailureEvidence::decode(&other).is_none());
        Ok(())
    }
}
