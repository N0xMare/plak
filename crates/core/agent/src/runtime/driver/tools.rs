//! Tool admission and bounded batch execution.

use crate::runtime::ArtifactTransition;
use crate::runtime::composition::projection::local_composition_exec_registry_arguments;
use crate::runtime::continuation::{ContinuationToolSlot, ToolBatchContinuation};
use crate::runtime::control::effect_intent_bytes;
use crate::runtime::driver::ToolDispatchOrigin;
use crate::runtime::driver::composition::LocalCompositionFailureEvidence;
use crate::runtime::driver::execution::{
    canonicalize_observed_tool_execution, execute_indexed_read_only, publish_observation_clip,
};
use crate::runtime::driver::{
    AdmittedToolCall, AdmittedToolExecution, AuthorizationOutcome, Driver,
    DurableEffectIntentEnvelope, IndexedReadOnlyRequest, IndexedToolFuture, ToolAdmission,
    ToolBatchResult,
};
use crate::runtime::observation::{
    bounded_rejection_detail, clip_tool_result, composition_tool_error_telemetry_status,
    conversation_item_tool_result, local_composition_result_observation,
    no_active_cell_wait_result, normalized_tool_failure, rejected_tool_call_observation,
    tool_call_observation, tool_result_content_bytes, tool_result_observation,
    tool_result_observation_with_subject, tool_result_telemetry_status,
};
use crate::runtime::operation_record::{
    canonical_tool_operation_binding, tool_operation_binding, tool_operation_terminal_state,
};
use crate::runtime::terminal::{DurableAgentError, cancelled_error};
use crate::runtime::{
    OperationId, PreparedToolInvocation, PromptCommand, ToolExecutionResult, ToolPresentation,
};
use crate::tool_runtime::{AuthorizedToolInvocation, PendingEffectDecision};
use crate::{
    AgentError, AgentErrorKind, AgentEventKind, DurableOperationKind, DurableOperationRecord,
    DurableOperationState, EventContext, SessionStoreError, StoreAdmission,
    TelemetryTerminalStatus, ToolAuditIdentity, ToolCallAttemptIdentity, ToolCallerLineageView,
};
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use plakous_model::{
    Actor, AttemptId, CallId, ContentBlock, ConversationItem, EntryId, HistorySegmentKind,
    ModelResponse, Provenance, TextBlock, ToolCallBlock, ToolEffect, ToolName, ToolResultBlock,
    ToolResultKind, ToolResultPart, ToolResultUpdateBlock, TurnId, ValidatedRequestPlan,
};
use plakous_tools::{
    ToolInvocation, ToolInvocationRequest, ToolOwnerClass, ToolProjectionReceiptV2,
    ToolRuntimeError, ToolRuntimeErrorKind,
};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
impl Driver {
    /// Closes the exchange of a batch that stopped with `error` and returns the
    /// error that ends the turn. An outcome-unknown stop keeps its working
    /// obligations for recovery instead, and an internal stop (a contained
    /// panic or broken driver invariant) does not rebuild history from state it
    /// can no longer vouch for. A close that itself fails on an
    /// unresolved effect ends the turn with that stronger error; any other close
    /// failure leaves the original failure in charge of the terminal.
    #[allow(
        clippy::too_many_arguments,
        reason = "the arguments are the exact interrupted batch and its provider round"
    )]
    pub(super) async fn close_interrupted_tool_batch(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        calls: &[ToolCallBlock],
        result_entry_ids: &[EntryId],
        provider_operation: OperationId,
        provider_response: &ModelResponse,
        restored_slots: Option<&[ContinuationToolSlot]>,
        error: AgentError,
    ) -> AgentError {
        if matches!(
            error.kind(),
            AgentErrorKind::OutcomeUnknown | AgentErrorKind::Internal
        ) {
            return error;
        }
        match self
            .promote_interrupted_tool_batch(
                command,
                attempt_id,
                calls,
                result_entry_ids,
                provider_operation,
                provider_response,
                restored_slots,
                &error,
            )
            .await
        {
            Err(close)
                if error.kind() == AgentErrorKind::Cancelled
                    || close.kind() == AgentErrorKind::OutcomeUnknown =>
            {
                close
            }
            _ => error,
        }
    }

    /// A batch interrupted by cancellation or an operational failure may have
    /// completed effects before it stopped. Rebuild from durable effect
    /// outcomes, not from published observations, and close every proposed call
    /// in one valid tool exchange before the root's terminal is published, so
    /// the next turn's model sees what already ran. A call that never completed
    /// gets a typed failure result naming `interruption`.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "one interruption close reconstructs every source slot and its durable operations"
    )]
    pub(super) async fn promote_interrupted_tool_batch(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        calls: &[ToolCallBlock],
        result_entry_ids: &[EntryId],
        provider_operation: OperationId,
        provider_response: &ModelResponse,
        restored_slots: Option<&[ContinuationToolSlot]>,
        interruption: &AgentError,
    ) -> Result<(), AgentError> {
        // Provider-programmatic calls carry a server continuation whose next
        // request has stricter adjacency rules than an ordinary local batch.
        if calls.iter().any(|call| {
            call.correlation()
                .caller_lineage()
                .provider_programmatic_kind()
                .is_some()
        }) {
            return Ok(());
        }
        let authority = self.authority.checkpoint();
        let mut effects = std::collections::BTreeMap::new();
        let mut markers = std::collections::BTreeMap::new();
        for record in authority.manifest().ledger().operations() {
            let (turn_id, recorded_attempt, call_id, source_index, marker) = match record.kind() {
                DurableOperationKind::Effect {
                    turn_id,
                    attempt_id,
                    call_id,
                    source_index,
                } => (turn_id, attempt_id, call_id, source_index, false),
                DurableOperationKind::CompositionEvaluation {
                    turn_id,
                    attempt_id,
                    call_id,
                    source_index,
                }
                | DurableOperationKind::CompositionContinuation {
                    turn_id,
                    attempt_id,
                    call_id,
                    source_index,
                    ..
                } => (turn_id, attempt_id, call_id, source_index, true),
                _ => continue,
            };
            if *turn_id == command.turn_id
                && *recorded_attempt == attempt_id
                && calls
                    .get(*source_index as usize)
                    .is_some_and(|call| call.call_id() == *call_id)
            {
                let target = if marker { &mut markers } else { &mut effects };
                if target
                    .insert(*source_index as usize, (*record).clone())
                    .is_some()
                {
                    return Err(AgentError::from_store(SessionStoreError::corrupt()));
                }
            }
        }
        let mut items = provider_response.output().to_vec();
        let mut operations = vec![provider_operation];
        for (index, (call, result_entry_id)) in calls.iter().zip(result_entry_ids).enumerate() {
            if let Some(record) = markers.remove(&index) {
                // A cell that yielded earlier in this batch is still live and
                // owned here: its marker already holds the yielded result, and
                // the root boundary terminates the cell and settles the marker.
                let live_yield = matches!(record.state(), DurableOperationState::LocatorBound(_))
                    && self.local_composition.as_ref().and_then(
                        crate::runtime::composition::CompositionOwner::evaluation_operation,
                    ) == Some(OperationId(record.operation_id()));
                let (item, nested_operations) =
                    self.cancelled_composition_result(call, *result_entry_id, &record, live_yield)?;
                if matches!(
                    record.state(),
                    DurableOperationState::Completed(_)
                        | DurableOperationState::ProvenNotDispatched(_)
                ) {
                    operations.push(OperationId(record.operation_id()));
                }
                operations.extend(nested_operations);
                items.push(item);
                continue;
            }
            let outcome = effects.remove(&index);
            let item = if let Some(record) = outcome {
                if matches!(record.state(), DurableOperationState::Prepared) {
                    let operation_id = OperationId(record.operation_id());
                    let terminal = tool_operation_terminal_state(
                        &Err(interruption.clone()),
                        false,
                        *result_entry_id,
                    )?;
                    self.transition_operation(command.turn_id, operation_id, terminal)
                        .await?;
                    operations.push(operation_id);
                    self.undispatched_tool_result(call, *result_entry_id, interruption)?
                } else {
                    let (DurableOperationState::Completed(evidence)
                    | DurableOperationState::ProvenNotDispatched(evidence)) = record.state()
                    else {
                        return Err(AgentError::new(
                            AgentErrorKind::OutcomeUnknown,
                            "interrupted tool batch contains an unresolved effect",
                        ));
                    };
                    operations.push(OperationId(record.operation_id()));
                    let (tag, evidence_entry, value): (String, EntryId, serde_json::Value) =
                        serde_json::from_slice(evidence.bytes())
                            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                    if evidence_entry != *result_entry_id {
                        return Err(AgentError::from_store(SessionStoreError::corrupt()));
                    }
                    if let Some(ContinuationToolSlot::Completed {
                        operation_id,
                        result_entry_id: restored_entry_id,
                        item,
                        ..
                    }) = restored_slots.and_then(|slots| slots.get(index))
                        && *operation_id == OperationId(record.operation_id())
                        && *restored_entry_id == *result_entry_id
                    {
                        item.clone()
                    } else if tag == "completed-tool-outcome-v1" {
                        let execution: ToolExecutionResult = serde_json::from_value(value)
                            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                        canonicalize_observed_tool_execution(
                            *result_entry_id,
                            execution,
                            &mut self.observation_budget,
                        )?
                        .0
                    } else if tag == "failed-tool-outcome-v1" {
                        let failure: DurableAgentError = serde_json::from_value(value)
                            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                        let failure = failure.into_error()?;
                        self.failed_tool_result(call, *result_entry_id, &failure)?
                    } else {
                        return Err(AgentError::from_store(SessionStoreError::corrupt()));
                    }
                }
            } else {
                self.undispatched_tool_result(call, *result_entry_id, interruption)?
            };
            items.push(item);
        }
        if !effects.is_empty() || !markers.is_empty() || calls.len() != result_entry_ids.len() {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
        self.promote_completed_operations_with_history(
            command.turn_id,
            Some(HistorySegmentKind::ToolExchange),
            items,
            operations,
            ArtifactTransition::Clear,
        )
        .await?;
        Ok(())
    }

    fn undispatched_tool_result(
        &mut self,
        call: &ToolCallBlock,
        result_entry_id: EntryId,
        interruption: &AgentError,
    ) -> Result<ConversationItem, AgentError> {
        let error = if interruption.kind() == AgentErrorKind::Cancelled {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Cancelled,
                "tool was not dispatched because the turn was cancelled",
            )
        } else {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                format!(
                    "tool was not dispatched because the turn failed ({:?})",
                    interruption.kind()
                ),
            )
        };
        canonicalize_observed_tool_execution(
            result_entry_id,
            ToolExecutionResult::new(normalized_tool_failure(call, &error)?, false),
            &mut self.observation_budget,
        )
        .map(|(item, _)| item)
    }

    fn failed_tool_result(
        &mut self,
        call: &ToolCallBlock,
        result_entry_id: EntryId,
        failure: &AgentError,
    ) -> Result<ConversationItem, AgentError> {
        let kind = if failure.kind() == AgentErrorKind::Cancelled {
            ToolRuntimeErrorKind::Cancelled
        } else {
            ToolRuntimeErrorKind::Execution
        };
        let error = ToolRuntimeError::new(kind, failure.message());
        canonicalize_observed_tool_execution(
            result_entry_id,
            ToolExecutionResult::new(normalized_tool_failure(call, &error)?, true),
            &mut self.observation_budget,
        )
        .map(|(item, _)| item)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the composition marker and nested evidence must be checked together"
    )]
    fn cancelled_composition_result(
        &mut self,
        call: &ToolCallBlock,
        result_entry_id: EntryId,
        record: &DurableOperationRecord,
        live_yield: bool,
    ) -> Result<(ConversationItem, Vec<OperationId>), AgentError> {
        let evidence = match record.state() {
            DurableOperationState::Completed(evidence)
            | DurableOperationState::ProvenNotDispatched(evidence) => evidence,
            DurableOperationState::LocatorBound(evidence) if live_yield => evidence,
            DurableOperationState::Finalized(_) => record
                .transitions()
                .iter()
                .rev()
                .find_map(|transition| match transition.state() {
                    DurableOperationState::Completed(evidence)
                    | DurableOperationState::ProvenNotDispatched(evidence) => Some(evidence),
                    _ => None,
                })
                .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?,
            _ => {
                return Err(AgentError::new(
                    AgentErrorKind::OutcomeUnknown,
                    "cancelled composition marker has an unresolved outcome",
                ));
            }
        };
        let (kind, parts, child_ids, updates) = if let Some(failure) =
            LocalCompositionFailureEvidence::decode(evidence.bytes())
        {
            const MAX_CANCELLED_REPORT_BYTES: usize = 1_048_576;
            let child_ids = failure.child_operations().to_vec();
            let detail = failure.detail();
            let mut report = format!("local composition was interrupted: {detail}");
            let ledger = self.authority.checkpoint();
            for child_id in &child_ids {
                let child = ledger
                    .manifest()
                    .ledger()
                    .operation(*child_id)
                    .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
                let (DurableOperationState::Completed(evidence)
                | DurableOperationState::ProvenNotDispatched(evidence)
                | DurableOperationState::Finalized(evidence)) = child.state()
                else {
                    return Err(AgentError::new(
                        AgentErrorKind::OutcomeUnknown,
                        "cancelled composition has an unresolved nested effect",
                    ));
                };
                if let DurableOperationKind::Effect { call_id, .. } = child.kind() {
                    let outcome: serde_json::Value = serde_json::from_slice(evidence.bytes())
                        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                    let outcome_fields = outcome
                        .as_array()
                        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
                    if outcome_fields.first().and_then(serde_json::Value::as_str)
                        == Some("completed-tool-outcome-v1")
                    {
                        let execution: ToolExecutionResult =
                            serde_json::from_value(outcome_fields.get(2).cloned().ok_or_else(
                                || AgentError::from_store(SessionStoreError::corrupt()),
                            )?)
                            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                        let rendered = serde_json::to_string(&execution.result)
                            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                        let line = format!("\ncompleted nested call {call_id}: {rendered}");
                        let remaining = MAX_CANCELLED_REPORT_BYTES.saturating_sub(report.len());
                        if remaining == 0 {
                            break;
                        }
                        let mut end = remaining.min(line.len());
                        while !line.is_char_boundary(end) {
                            end -= 1;
                        }
                        report.push_str(&line[..end]);
                        if end < line.len() {
                            report.push_str("\n[nested results truncated]");
                            break;
                        }
                    }
                }
            }
            (
                ToolResultKind::Failure,
                vec![ToolResultPart::Text(TextBlock::new(report, None).map_err(
                    |_| AgentError::from_store(SessionStoreError::corrupt()),
                )?)],
                child_ids,
                Vec::new(),
            )
        } else {
            let value: serde_json::Value = serde_json::from_slice(evidence.bytes())
                .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
            let fields = value
                .as_array()
                .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
            let tag = fields
                .first()
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
            match tag {
                "plakous-local-composition-cell-v2" if fields.len() == 8 => {
                    let child_ids: Vec<u64> = serde_json::from_value(fields[4].clone())
                        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                    let parts: Vec<ToolResultPart> = serde_json::from_value(fields[7].clone())
                        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                    let updates: Vec<ToolResultUpdateBlock> =
                        serde_json::from_value(fields[6].clone())
                            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                    (ToolResultKind::Success, parts, child_ids, updates)
                }
                _ => return Err(AgentError::from_store(SessionStoreError::corrupt())),
            }
        };
        let result = ToolResultBlock::new(
            call.call_id(),
            call.owner().clone(),
            kind,
            parts,
            call.correlation().clone(),
        )
        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
        let (result, _) = clip_tool_result(result, &mut self.observation_budget)?;
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
        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
        let ledger = self.authority.checkpoint();
        let mut operations = Vec::new();
        for child_id in child_ids {
            let child = ledger
                .manifest()
                .ledger()
                .operation(child_id)
                .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
            match child.state() {
                DurableOperationState::Completed(_)
                | DurableOperationState::ProvenNotDispatched(_) => {
                    operations.push(OperationId(child_id));
                }
                DurableOperationState::Finalized(_) => {}
                _ => {
                    return Err(AgentError::new(
                        AgentErrorKind::OutcomeUnknown,
                        "cancelled composition has an unresolved nested operation",
                    ));
                }
            }
        }
        Ok((item, operations))
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "source-ordered admission, mutation barriers, and indexed read-only joining remain visible in one auditable batch"
    )]
    pub(super) async fn execute_tool_batch(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        calls: Vec<ToolCallBlock>,
        result_entry_ids: Vec<EntryId>,
        tool_calls: usize,
        provider_operation: OperationId,
        provider_plan: &ValidatedRequestPlan,
        provider_response: &ModelResponse,
    ) -> Result<ToolBatchResult, AgentError> {
        let batch = ToolBatchInput {
            command,
            attempt_id,
            calls: &calls,
            result_entry_ids: &result_entry_ids,
            tool_calls,
            provider_operation,
            provider_plan,
            provider_response,
        };
        let mut progress = ToolBatchProgress {
            items: vec![None; calls.len()],
            read_only: Vec::new(),
            dispatched: 0,
            operations: Vec::new(),
        };
        for (index, call) in calls.iter().cloned().enumerate() {
            if command.cancellation.is_cancelled() || self.shutdown.is_cancelled() {
                command.cancellation.cancel();
                return Err(cancelled_error());
            }
            let source_index = u32::try_from(index).map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "tool source index exceeded the durable manifest bound",
                )
            })?;
            match Box::pin(self.admit_tool(command, attempt_id, &call)).await? {
                ToolAdmission::RejectedBeforeValidation {
                    execution,
                    attempt,
                    reason,
                    detail,
                } => {
                    let result_entry_id = result_entry_ids[index];
                    let operation_id = self
                        .prepare_rejected_tool_operation(
                            command.turn_id,
                            attempt_id,
                            source_index,
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
                    let _ = command.shared.events.publish(
                        EventContext::call(command.turn_id, attempt_id, call.call_id()),
                        AgentEventKind::ToolPreparationRejected {
                            attempt,
                            lineage: self.tool_caller_lineage(&call),
                            reason,
                            detail: Some(Arc::from(bounded_rejection_detail(&detail))),
                            // A rejected call never starts, so nothing else in
                            // the stream says what it sent.
                            observation: Some(rejected_tool_call_observation(&call)),
                        },
                    );
                    progress.items[index] = Some(item);
                    progress.operations.push(operation_id);
                }
                ToolAdmission::RejectedAfterValidation { execution } => {
                    let result_entry_id = result_entry_ids[index];
                    let operation_id = self
                        .prepare_rejected_tool_operation(
                            command.turn_id,
                            attempt_id,
                            source_index,
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
                    progress.items[index] = Some(item);
                    progress.operations.push(operation_id);
                }
                ToolAdmission::Admitted {
                    execution,
                    audit_identity,
                } => {
                    Box::pin(self.dispatch_admitted_tool(
                        batch,
                        &mut progress,
                        index,
                        source_index,
                        call,
                        execution,
                        audit_identity,
                    ))
                    .await?;
                }
                ToolAdmission::DecisionRequired {
                    invocation,
                    projection_receipt,
                    pending,
                    audit_identity,
                } => {
                    Box::pin(self.dispatch_decision_required_tool(
                        batch,
                        &mut progress,
                        index,
                        source_index,
                        call,
                        invocation,
                        projection_receipt,
                        pending,
                        audit_identity,
                    ))
                    .await?;
                }
            }
        }
        if !progress.read_only.is_empty() {
            let parent_call_id = self.active_composition_marker_call();
            progress.dispatched = progress.dispatched.saturating_add(
                Box::pin(self.execute_read_only_group(
                    command,
                    attempt_id,
                    progress.read_only,
                    &mut progress.items,
                    ToolDispatchOrigin::TopLevel,
                    parent_call_id,
                ))
                .await?,
            );
        }
        let items = progress
            .items
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "tool batch completed without every source-ordered result",
                )
            })?;
        Ok(ToolBatchResult {
            items,
            dispatched: progress.dispatched,
            operations: progress.operations,
        })
    }

    /// Executes or joins one admitted call in source order.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "one source-ordered call keeps its barriers and joins in a single auditable body"
    )]
    async fn dispatch_admitted_tool(
        &mut self,
        batch: ToolBatchInput<'_>,
        progress: &mut ToolBatchProgress,
        index: usize,
        source_index: u32,
        call: ToolCallBlock,
        execution: Box<AdmittedToolExecution>,
        audit_identity: ToolAuditIdentity,
    ) -> Result<(), AgentError> {
        let ToolBatchInput {
            command,
            attempt_id,
            result_entry_ids,
            ..
        } = batch;
        if self.is_local_composition_marker(&execution) {
            let result_entry_id = result_entry_ids[index];
            // Outer exec_*/wait_* skip ToolRuntime, so drive the
            // same observer Direct tools use: one ToolStarted and
            // one ToolFinished per model-visible call. Nested calls
            // inside the cell are separate calls and observe
            // themselves on the ordinary dispatch paths.
            let observer = self.tool_dispatch_observer(
                command,
                attempt_id,
                call.call_id(),
                audit_identity.clone(),
                self.local_composition_marker_lineage(),
                tool_call_observation(&call),
            );
            observer.observe_started()?;
            let local_composition = match Box::pin(self.execute_local_composition_outer(
                command,
                attempt_id,
                source_index,
                &call,
                result_entry_id,
                *execution,
            ))
            .await
            {
                Ok(local_composition) => local_composition,
                Err(error) => {
                    observer.emit_finished(
                        composition_tool_error_telemetry_status(
                            &error,
                            command.cancellation.is_cancelled() || self.shutdown.is_cancelled(),
                        ),
                        0,
                    );
                    if let Some(error) = observer.take_error() {
                        return Err(error);
                    }
                    return Err(error);
                }
            };
            let output_bytes = conversation_item_tool_result(&local_composition.item)
                .map_or(0, tool_result_content_bytes);
            let status = conversation_item_tool_result(&local_composition.item)
                .map_or(TelemetryTerminalStatus::Failed, |result| {
                    tool_result_telemetry_status(result, self.shutdown.is_cancelled())
                });
            observer.emit_finished(status, output_bytes);
            if let Some(error) = observer.take_error() {
                return Err(error);
            }
            let _ = command.shared.events.publish(
                EventContext::call(command.turn_id, attempt_id, call.call_id()),
                AgentEventKind::ToolCompleted {
                    identity: audit_identity.clone(),
                    lineage: self.local_composition_marker_terminal_lineage(),
                    observation: local_composition_result_observation(&local_composition.item),
                },
            );
            progress.dispatched = progress
                .dispatched
                .saturating_add(local_composition.dispatched);
            progress.items[index] = Some(local_composition.item);
            progress.operations.extend(local_composition.operations);
            return Ok(());
        }
        let kind = DurableOperationKind::Effect {
            turn_id: command.turn_id,
            attempt_id,
            call_id: call.call_id(),
            source_index,
        };
        match *execution {
            AdmittedToolExecution::ReadOnly {
                invocation,
                projection_receipt,
                authorized,
                effect,
            } => {
                let effect_binding = canonical_tool_operation_binding(index, &invocation)?;
                let recoverable_input =
                    serde_json::to_vec(&(effect_binding, &call)).map_err(|_| {
                        AgentError::new(
                            AgentErrorKind::Internal,
                            "tool operation input could not be captured",
                        )
                    })?;
                let operation_id = self
                    .prepare_operation(
                        StoreAdmission::Ordinary,
                        command.turn_id,
                        kind,
                        effect_binding,
                        Some(recoverable_input),
                        None,
                    )
                    .await?;
                progress.read_only.push(AdmittedToolCall {
                    operation_id,
                    index,
                    call,
                    result_entry_id: result_entry_ids[index],
                    audit_identity,
                    execution: AdmittedToolExecution::ReadOnly {
                        invocation,
                        projection_receipt,
                        authorized,
                        effect,
                    },
                });
                progress.operations.push(operation_id);
            }
            AdmittedToolExecution::Mutating {
                invocation,
                projection_receipt,
                authorized,
                intent,
            } => {
                self.certify_mutating_effect_recovery()?;
                let recoverable_input = serde_json::to_vec(&DurableEffectIntentEnvelope {
                    intent_digest: intent.digest(),
                    call: call.clone(),
                    intent: effect_intent_bytes(&intent)?,
                })
                .map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "mutating effect intent could not be captured",
                    )
                })?;
                let operation_id = self
                    .prepare_operation(
                        StoreAdmission::Ordinary,
                        command.turn_id,
                        kind,
                        intent.digest(),
                        Some(recoverable_input),
                        None,
                    )
                    .await?;
                let claimed = self
                    .claim_mutating_effect(command.turn_id, operation_id, *intent, *authorized)
                    .await?;
                let result_entry_id = result_entry_ids[index];
                let parent_call_id = self.active_composition_marker_call();
                let execution = Box::pin(self.execute_mutating_effect(
                    command,
                    attempt_id,
                    operation_id,
                    *invocation,
                    projection_receipt,
                    claimed,
                    &call,
                    &audit_identity,
                    result_entry_id,
                    ToolDispatchOrigin::TopLevel,
                    parent_call_id,
                ))
                .await?;
                progress.dispatched = progress
                    .dispatched
                    .saturating_add(usize::from(execution.dispatched));
                progress.items[index] = Some(execution.item);
                progress.operations.push(operation_id);
            }
        }
        Ok(())
    }

    /// Pauses one call at its effect decision, then executes or rejects it.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "one source-ordered call keeps its barriers and joins in a single auditable body"
    )]
    async fn dispatch_decision_required_tool(
        &mut self,
        batch: ToolBatchInput<'_>,
        progress: &mut ToolBatchProgress,
        index: usize,
        source_index: u32,
        call: ToolCallBlock,
        invocation: Box<ToolInvocation>,
        projection_receipt: Option<ToolProjectionReceiptV2>,
        pending: Box<PendingEffectDecision>,
        audit_identity: ToolAuditIdentity,
    ) -> Result<(), AgentError> {
        let ToolBatchInput {
            command,
            attempt_id,
            calls,
            result_entry_ids,
            tool_calls,
            provider_operation,
            provider_plan,
            provider_response,
            ..
        } = batch;
        if self.is_local_composition_invocation(&invocation) {
            drop(pending);
            let error = ToolRuntimeError::new(
                ToolRuntimeErrorKind::Policy,
                "local composition nested continuation cannot pause safely for a decision",
            );
            let execution =
                ToolExecutionResult::new(normalized_tool_failure(&call, &error)?, false);
            let operation_id = self
                .prepare_rejected_tool_operation(
                    command.turn_id,
                    attempt_id,
                    source_index,
                    &call,
                    result_entry_ids[index],
                    &execution,
                )
                .await?;
            let (item, clip) = canonicalize_observed_tool_execution(
                result_entry_ids[index],
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

            progress.items[index] = Some(item);
            progress.operations.push(operation_id);
            return Ok(());
        }
        if !progress.read_only.is_empty() {
            let parent_call_id = self.active_composition_marker_call();
            progress.dispatched = progress.dispatched.saturating_add(
                Box::pin(self.execute_read_only_group(
                    command,
                    attempt_id,
                    std::mem::take(&mut progress.read_only),
                    &mut progress.items,
                    ToolDispatchOrigin::TopLevel,
                    parent_call_id,
                ))
                .await?,
            );
        }
        let continuation = ToolBatchContinuation {
            tool_calls,
            provider_operation,
            provider_plan,
            provider_response,
            calls,
            result_entry_ids,
            items: &progress.items,
            operations: &progress.operations,
            source_index: index,
        };
        let authorization = Box::pin(self.await_effect_decision(
            command,
            attempt_id,
            &call,
            *pending,
            &continuation,
        ))
        .await?;
        let authorization = if *self.tool_preempt.borrow() == plakous_tools::ToolPreempt::Steer
            && matches!(authorization, AuthorizationOutcome::Authorized(_))
        {
            AuthorizationOutcome::Rejected(self.superseded_tool_execution(&call)?.result)
        } else {
            authorization
        };
        match authorization {
            AuthorizationOutcome::Rejected(result) => {
                let execution = ToolExecutionResult::new(result, false);
                let operation_id = self
                    .prepare_rejected_tool_operation(
                        command.turn_id,
                        attempt_id,
                        source_index,
                        &call,
                        result_entry_ids[index],
                        &execution,
                    )
                    .await?;
                let (item, clip) = canonicalize_observed_tool_execution(
                    result_entry_ids[index],
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

                progress.items[index] = Some(item);
                progress.operations.push(operation_id);
            }
            AuthorizationOutcome::Authorized(authorized) => {
                let _ = command.shared.events.publish(
                    EventContext::call(command.turn_id, attempt_id, call.call_id()),
                    AgentEventKind::ToolAuthorized {
                        identity: audit_identity.clone(),
                        lineage: self.tool_caller_lineage(&call),
                    },
                );
                let execution =
                    self.admitted_tool_execution(*invocation, projection_receipt, *authorized)?;
                let kind = DurableOperationKind::Effect {
                    turn_id: command.turn_id,
                    attempt_id,
                    call_id: call.call_id(),
                    source_index,
                };
                match execution {
                    AdmittedToolExecution::ReadOnly {
                        invocation,
                        projection_receipt,
                        authorized,
                        effect,
                    } => {
                        let effect_binding = canonical_tool_operation_binding(index, &invocation)?;
                        let input = serde_json::to_vec(&(effect_binding, &call)).map_err(|_| {
                            AgentError::new(
                                AgentErrorKind::Internal,
                                "tool operation input could not be captured",
                            )
                        })?;
                        let operation_id = self
                            .prepare_operation(
                                StoreAdmission::Ordinary,
                                command.turn_id,
                                kind,
                                effect_binding,
                                Some(input),
                                None,
                            )
                            .await?;
                        progress.read_only.push(AdmittedToolCall {
                            operation_id,
                            index,
                            call,
                            result_entry_id: result_entry_ids[index],
                            audit_identity,
                            execution: AdmittedToolExecution::ReadOnly {
                                invocation,
                                projection_receipt,
                                authorized,
                                effect,
                            },
                        });
                        progress.operations.push(operation_id);
                    }
                    AdmittedToolExecution::Mutating {
                        invocation,
                        projection_receipt,
                        authorized,
                        intent,
                    } => {
                        self.certify_mutating_effect_recovery()?;
                        let input = serde_json::to_vec(&DurableEffectIntentEnvelope {
                            intent_digest: intent.digest(),
                            call: call.clone(),
                            intent: effect_intent_bytes(&intent)?,
                        })
                        .map_err(|_| {
                            AgentError::new(
                                AgentErrorKind::Internal,
                                "mutating effect intent could not be captured",
                            )
                        })?;
                        let operation_id = self
                            .prepare_operation(
                                StoreAdmission::Ordinary,
                                command.turn_id,
                                kind,
                                intent.digest(),
                                Some(input),
                                None,
                            )
                            .await?;
                        let claimed = self
                            .claim_mutating_effect(
                                command.turn_id,
                                operation_id,
                                *intent,
                                *authorized,
                            )
                            .await?;
                        let parent_call_id = self.active_composition_marker_call();
                        let execution = Box::pin(self.execute_mutating_effect(
                            command,
                            attempt_id,
                            operation_id,
                            *invocation,
                            projection_receipt,
                            claimed,
                            &call,
                            &audit_identity,
                            result_entry_ids[index],
                            ToolDispatchOrigin::TopLevel,
                            parent_call_id,
                        ))
                        .await?;
                        progress.dispatched = progress
                            .dispatched
                            .saturating_add(usize::from(execution.dispatched));
                        progress.items[index] = Some(execution.item);
                        progress.operations.push(operation_id);
                    }
                }
            }
            AuthorizationOutcome::Pending(_) => {
                return Err(AgentError::new(
                    AgentErrorKind::Internal,
                    "resolved decision remained pending",
                ));
            }
        }
        Ok(())
    }

    pub(super) async fn prepare_rejected_tool_operation(
        &mut self,
        turn_id: TurnId,
        attempt_id: AttemptId,
        source_index: u32,
        call: &ToolCallBlock,
        result_entry_id: EntryId,
        execution: &ToolExecutionResult,
    ) -> Result<OperationId, AgentError> {
        let binding = tool_operation_binding(
            turn_id,
            attempt_id,
            usize::try_from(source_index).map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "tool source index could not be reconstructed",
                )
            })?,
            call,
        )?;
        let request: super::DurableUndispatchedToolRequest = (
            "proven-not-dispatched-tool-v1".to_owned(),
            binding,
            call.clone(),
            result_entry_id,
            execution.clone(),
        );
        let input = serde_json::to_vec(&request).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "rejected tool outcome could not be captured",
            )
        })?;
        let operation_id = self
            .prepare_operation(
                StoreAdmission::Settlement,
                turn_id,
                DurableOperationKind::Effect {
                    turn_id,
                    attempt_id,
                    call_id: call.call_id(),
                    source_index,
                },
                binding,
                Some(input),
                None,
            )
            .await?;
        self.transition_operation(
            turn_id,
            operation_id,
            tool_operation_terminal_state(&Ok(execution.clone()), false, result_entry_id)?,
        )
        .await?;
        Ok(operation_id)
    }

    /// Records the canonical name of every runtime-owned call in a provider
    /// response as it lands, before anything records or classifies it, and
    /// stamps the frozen registry's owner of that name onto the call. This is
    /// the one point a provider-emitted call becomes canonical; its results
    /// take the same owner from the invoked registration or from the call.
    pub(super) fn land_tool_calls(
        &self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        response: ModelResponse,
    ) -> ModelResponse {
        response.with_canonical_tool_calls(|call| {
            let name = self.canonical_tool_name(command, attempt_id, &self.tool_presentation, call);
            let server = match self.tools.owner(&name) {
                Some(ToolOwnerClass::Mcp(server)) => Some(server.canonical().clone()),
                Some(ToolOwnerClass::PlakLocal) | None => None,
            };
            (name, server)
        })
    }

    /// Resolves one call's canonical name through `presentation`, preparing it
    /// with the request admission builds when its exposure needs arguments.
    pub(super) fn canonical_tool_name(
        &self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        presentation: &ToolPresentation,
        call: &ToolCallBlock,
    ) -> ToolName {
        presentation.canonical_name(call, |presentation| {
            let arguments = presentation.registry_arguments(call)?;
            presentation.prepare(
                ToolInvocationRequest::new(
                    self.run_id,
                    command.turn_id,
                    attempt_id,
                    call.call_id(),
                    call.exposed_name().clone(),
                    call.correlation().clone(),
                    Arc::<str>::from(arguments),
                ),
                self.tools.environment(),
            )
        })
    }

    pub(super) async fn admit_tool(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        call: &ToolCallBlock,
    ) -> Result<ToolAdmission, AgentError> {
        if *self.tool_preempt.borrow() == plakous_tools::ToolPreempt::Steer {
            return Ok(ToolAdmission::RejectedAfterValidation {
                execution: self.superseded_tool_execution(call)?,
            });
        }
        if self.local_composition.as_ref().is_some_and(|state| {
            call.exposed_name().as_str() == state.descriptor().wait_name.as_ref()
                && state.evaluation_operation().is_none()
        }) {
            // Asking what a cell is doing when none is running is a question
            // with an answer, not a malformed call. Rejecting it punished a
            // model for checking, and cost the turn a tool call it could learn
            // nothing from. Answer it: nothing is running, so there is nothing
            // to wait for. The call is recorded as proven not dispatched.
            //
            // The question still has to be well formed. A malformed `wait` is
            // malformed whether or not a cell is running, so the presented
            // contract validates it first and an invalid one is refused exactly
            // as it would have been mid-evaluation. `"none"`, `"null"` and
            // `"all"` are ordinary non-empty strings, so every such spelling
            // validates and is answered; an absent or empty
            // `cell_id` is refused for the field it omits, which is what the
            // model needs to hear when there is no cell to name either.
            let request = ToolInvocationRequest::new(
                self.run_id,
                command.turn_id,
                attempt_id,
                call.call_id(),
                call.exposed_name().clone(),
                call.correlation().clone(),
                Arc::<str>::from(call.raw_arguments()),
            );
            let prepared = match self
                .tool_presentation
                .clone()
                .prepare(request, self.tools.environment())
            {
                Ok(prepared) => prepared,
                Err(error) => {
                    return Ok(ToolAdmission::RejectedBeforeValidation {
                        execution: ToolExecutionResult::new(
                            normalized_tool_failure(call, &error)?,
                            false,
                        ),
                        attempt: ToolCallAttemptIdentity::new(
                            call.exposed_name().clone(),
                            Some(self.tool_presentation.digest()),
                        ),
                        reason: error.kind(),
                        detail: error.to_string(),
                    });
                }
            };
            let result = no_active_cell_wait_result(call)?;
            // The model is told this call succeeded, so every face says so too.
            // Without this the call appears on the JSONL and TUI streams with no
            // result line, which reads as a call that never finished.
            let _ = command.shared.events.publish(
                EventContext::call(command.turn_id, attempt_id, call.call_id()),
                AgentEventKind::ToolCompleted {
                    identity: ToolAuditIdentity::from_invocation(&prepared.invocation),
                    lineage: self.tool_caller_lineage(call),
                    observation: Some(tool_result_observation(&result)),
                },
            );
            return Ok(ToolAdmission::RejectedAfterValidation {
                execution: ToolExecutionResult::new(result, false),
            });
        }
        if self.local_composition.as_ref().is_some_and(|state| {
            call.exposed_name().as_str() == state.descriptor().execute_name.as_ref()
        }) {
            return self
                .admit_local_composition_exec(command, attempt_id, call)
                .await;
        }
        let parent_call_id = self.active_composition_marker_call();
        self.admit_tool_with_presentation(
            command,
            attempt_id,
            call,
            self.tool_presentation.clone(),
            ToolDispatchOrigin::TopLevel,
            parent_call_id,
        )
        .await
    }

    async fn admit_local_composition_exec(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        call: &ToolCallBlock,
    ) -> Result<ToolAdmission, AgentError> {
        let definition = self
            .tools
            .definitions()
            .iter()
            .find(|definition| definition.name() == call.canonical_name())
            .ok_or_else(|| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "local composition exec marker is absent from its frozen registry",
                )
            })?;
        let registry_arguments =
            match local_composition_exec_registry_arguments(call, definition.input()) {
                Ok(arguments) => arguments,
                Err(error) => {
                    return Ok(ToolAdmission::RejectedBeforeValidation {
                        execution: ToolExecutionResult::new(
                            normalized_tool_failure(call, &error)?,
                            false,
                        ),
                        attempt: ToolCallAttemptIdentity::new(
                            call.exposed_name().clone(),
                            Some(self.tool_presentation.digest()),
                        ),
                        reason: error.kind(),
                        detail: error.to_string(),
                    });
                }
            };
        let request = ToolInvocationRequest::new(
            self.run_id,
            command.turn_id,
            attempt_id,
            call.call_id(),
            call.canonical_name().clone(),
            call.correlation().clone(),
            Arc::<str>::from(registry_arguments),
        );
        let invocation = match self.tools.prepare(request) {
            Ok(invocation) => invocation,
            Err(error) => {
                return Ok(ToolAdmission::RejectedBeforeValidation {
                    execution: ToolExecutionResult::new(
                        normalized_tool_failure(call, &error)?,
                        false,
                    ),
                    attempt: ToolCallAttemptIdentity::new(
                        call.exposed_name().clone(),
                        Some(self.tool_presentation.digest()),
                    ),
                    reason: error.kind(),
                    detail: error.to_string(),
                });
            }
        };
        if !self.is_local_composition_invocation(&invocation) {
            return Err(AgentError::new(
                AgentErrorKind::Internal,
                "exec did not resolve to the exact local composition marker binding",
            ));
        }
        let audit_identity = ToolAuditIdentity::from_invocation(&invocation);
        match self
            .authorize_tool(command, call, invocation.clone())
            .await?
        {
            AuthorizationOutcome::Authorized(authorized) => {
                let _ = command.shared.events.publish(
                    EventContext::call(command.turn_id, attempt_id, call.call_id()),
                    AgentEventKind::ToolAuthorized {
                        identity: audit_identity.clone(),
                        lineage: self.local_composition_marker_lineage(),
                    },
                );
                Ok(ToolAdmission::Admitted {
                    execution: Box::new(self.admitted_tool_execution(
                        invocation,
                        None,
                        *authorized,
                    )?),
                    audit_identity,
                })
            }
            AuthorizationOutcome::Pending(pending) => Ok(ToolAdmission::DecisionRequired {
                invocation: Box::new(invocation),
                projection_receipt: None,
                pending,
                audit_identity,
            }),
            AuthorizationOutcome::Rejected(result) => Ok(ToolAdmission::RejectedAfterValidation {
                execution: ToolExecutionResult::new(result, false),
            }),
        }
    }

    /// Admits one call, publishing its authorization with caller lineage.
    ///
    /// `parent_call_id` names the outer local-composition marker call when the
    /// caller is a cell. The nested scheduler borrows the driver out of `self`,
    /// so it supplies the parent explicitly rather than through `self`.
    pub(super) async fn admit_tool_with_presentation(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        call: &ToolCallBlock,
        presentation: ToolPresentation,
        origin: ToolDispatchOrigin,
        parent_call_id: Option<CallId>,
    ) -> Result<ToolAdmission, AgentError> {
        // The composition `wait` markers accept "no particular cell", which
        // only the driver can lower, because only the driver knows what is
        // running. Every other tool, whatever its spelling, passes through
        // untouched.
        let normalized = presentation
            .is_cell_wait_marker(call.exposed_name())
            .then(|| {
                plakous_tools::normalized_wait_arguments(
                    call.raw_arguments(),
                    self.local_composition
                        .as_ref()
                        .and_then(|state| state.cell()),
                )
            })
            .flatten();
        let prepared = match presentation.registry_arguments(call).and_then(|arguments| {
            let request = ToolInvocationRequest::new(
                self.run_id,
                command.turn_id,
                attempt_id,
                call.call_id(),
                call.exposed_name().clone(),
                call.correlation().clone(),
                Arc::<str>::from(normalized.as_deref().unwrap_or(arguments)),
            );
            presentation.prepare(request, self.tools.environment())
        }) {
            Ok(prepared) => prepared,
            Err(error) => {
                return Ok(ToolAdmission::RejectedBeforeValidation {
                    execution: ToolExecutionResult::new(
                        normalized_tool_failure(call, &error)?,
                        false,
                    ),
                    attempt: ToolCallAttemptIdentity::new(
                        call.exposed_name().clone(),
                        Some(presentation.digest()),
                    ),
                    reason: error.kind(),
                    detail: error.to_string(),
                });
            }
        };
        let PreparedToolInvocation {
            invocation,
            projection_receipt,
        } = prepared;
        let audit_identity = ToolAuditIdentity::from_invocation(&invocation);
        let authorization =
            Box::pin(self.authorize_tool(command, call, invocation.clone())).await?;
        let authorized = match authorization {
            AuthorizationOutcome::Authorized(authorized) => authorized,
            AuthorizationOutcome::Pending(pending) => {
                return Ok(ToolAdmission::DecisionRequired {
                    invocation: Box::new(invocation),
                    projection_receipt,
                    pending,
                    audit_identity,
                });
            }
            AuthorizationOutcome::Rejected(result) => {
                return Ok(ToolAdmission::RejectedAfterValidation {
                    execution: ToolExecutionResult::new(result, false),
                });
            }
        };
        if origin == ToolDispatchOrigin::TopLevel
            && *self.tool_preempt.borrow() == plakous_tools::ToolPreempt::Steer
        {
            return Ok(ToolAdmission::RejectedAfterValidation {
                execution: self.superseded_tool_execution(call)?,
            });
        }
        let _ = command.shared.events.publish(
            EventContext::call(command.turn_id, attempt_id, call.call_id()),
            AgentEventKind::ToolAuthorized {
                identity: audit_identity.clone(),
                lineage: ToolCallerLineageView::from_lineage(
                    call.correlation().caller_lineage(),
                    parent_call_id,
                ),
            },
        );
        let execution =
            self.admitted_tool_execution(invocation, projection_receipt, *authorized)?;
        Ok(ToolAdmission::Admitted {
            execution: Box::new(execution),
            audit_identity,
        })
    }

    pub(super) fn admitted_tool_execution(
        &self,
        invocation: ToolInvocation,
        projection_receipt: Option<ToolProjectionReceiptV2>,
        authorized: AuthorizedToolInvocation,
    ) -> Result<AdmittedToolExecution, AgentError> {
        let effect = self
            .tools
            .definitions()
            .iter()
            .find(|definition| definition.name() == invocation.identity().name())
            .map(plakous_model::ToolDefinition::effect)
            .ok_or_else(|| {
                let message = if self.tools.definitions().is_empty() {
                    "authorized tool registry is empty"
                } else {
                    "authorized tool identity is absent from the registry"
                };
                AgentError::new(AgentErrorKind::Internal, message)
            })?;
        Ok(match effect {
            ToolEffect::ReadOnly => AdmittedToolExecution::ReadOnly {
                invocation: Box::new(invocation),
                projection_receipt,
                authorized: Box::new(authorized),
                effect,
            },
            ToolEffect::Mutating => {
                let intent = self
                    .tools
                    .effect_dispatch_intent(&authorized)
                    .map_err(|error| AgentError::from_tool(&error))?;
                AdmittedToolExecution::Mutating {
                    invocation: Box::new(invocation),
                    projection_receipt,
                    authorized: Box::new(authorized),
                    intent: Box::new(intent),
                }
            }
            _ => {
                return Err(AgentError::new(
                    AgentErrorKind::InvalidInput,
                    "authorized tool uses an unsupported effect class",
                ));
            }
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the actor-owned select must visibly join every indexed execution while retaining controls and lease renewal"
    )]
    #[allow(
        clippy::too_many_arguments,
        reason = "the arguments are the exact admitted read-only group and its caller lineage"
    )]
    pub(super) async fn execute_read_only_group(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        calls: Vec<AdmittedToolCall>,
        items: &mut [Option<ConversationItem>],
        origin: ToolDispatchOrigin,
        parent_call_id: Option<CallId>,
    ) -> Result<usize, AgentError> {
        let mut executions = FuturesUnordered::<IndexedToolFuture>::new();
        if origin == ToolDispatchOrigin::TopLevel
            && *self.tool_preempt.borrow() == plakous_tools::ToolPreempt::Steer
        {
            for call in calls {
                let execution = self.superseded_tool_execution(&call.call)?;
                let terminal = tool_operation_terminal_state(
                    &Ok(execution.clone()),
                    false,
                    call.result_entry_id,
                )?;
                self.transition_operation(command.turn_id, call.operation_id, terminal)
                    .await?;
                let (item, _) = canonicalize_observed_tool_execution(
                    call.result_entry_id,
                    execution,
                    &mut self.observation_budget,
                )?;
                items[call.index] = Some(item);
            }
            return Ok(0);
        }
        // One commit dispatches the whole group, so an ordinary refusal leaves
        // every call `Prepared` rather than some dispatched and never run.
        self.transition_operations(
            command.turn_id,
            calls
                .iter()
                .map(|call| (call.operation_id, DurableOperationState::Dispatched))
                .collect(),
        )
        .await?;
        for call in calls {
            let AdmittedToolExecution::ReadOnly {
                invocation,
                projection_receipt,
                authorized,
                effect: _,
            } = call.execution
            else {
                return Err(AgentError::new(
                    AgentErrorKind::Internal,
                    "mutating tool entered the read-only execution group",
                ));
            };
            let lineage = ToolCallerLineageView::from_lineage(
                call.call.correlation().caller_lineage(),
                parent_call_id,
            );
            let dispatch_witness = Arc::new(AtomicBool::new(false));
            let process_session_update = Arc::new(Mutex::new(None));
            let telemetry_context = self
                .telemetry_context
                .with_turn(command.turn_id)
                .with_attempt(attempt_id)
                .with_call(call.call.call_id());
            let (preempt, preempt_guard) = if origin == ToolDispatchOrigin::TopLevel {
                let (preempt, guard) = self.dispatch_preempt();
                (preempt, Some(guard))
            } else {
                let (preempt, guard) = self.nested_notice_preempt();
                (preempt, Some(guard))
            };
            executions.push(Box::pin(execute_indexed_read_only(
                IndexedReadOnlyRequest {
                    authority: self.start_authority(),
                    operation_id: call.operation_id,
                    tools: self.tools.clone(),
                    tool_presentation: self.tool_presentation.clone(),
                    invocation: *invocation,
                    projection_receipt,
                    authorized: *authorized,
                    audit_identity: call.audit_identity,
                    lineage,
                    index: call.index,
                    call: call.call,
                    result_entry_id: call.result_entry_id,
                    cancellation: command.cancellation.clone(),
                    shutdown: self.shutdown.clone(),
                    preempt,
                    preempt_guard,
                    clock: Arc::clone(&self.clock),
                    progress_capacity: self.config.tool_progress_capacity,
                    events: command.shared.events.clone(),
                    turn_id: command.turn_id,
                    attempt_id,
                    telemetry: Arc::clone(&self.telemetry),
                    telemetry_context,
                    turn_shared: Arc::clone(&command.shared),
                    dispatch_witness,
                    process_session_update,
                },
            )));
        }

        let mut controls_open = true;
        let mut boundary_error = None;
        let mut completed = (0..items.len()).map(|_| None).collect::<Vec<_>>();
        let mut dispatched = 0_usize;

        let mut job_changes = self.job_sessions.job_terminal_changes();
        job_changes.mark_changed();
        while !executions.is_empty() {
            tokio::select! {
                biased;
                () = self.authority.lease_lost(), if boundary_error.is_none() => {
                    command.cancellation.cancel();
                    boundary_error = Some(self.authority.heartbeat_failure());
                }
                () = command.cancellation.cancelled(), if boundary_error.is_none() => {
                    command.cancellation.cancel();
                    boundary_error = Some(cancelled_error());
                }
                () = self.shutdown.cancelled(), if boundary_error.is_none() => {
                    command.cancellation.cancel();
                    boundary_error = Some(cancelled_error());
                }
                control = self.control_receiver.recv(), if controls_open => {
                    if let Some(control) = control {
                        Box::pin(self.handle_control(Some(command), control)).await;
                    } else {
                        controls_open = false;
                        command.cancellation.cancel();
                        if boundary_error.is_none() {
                            boundary_error = Some(cancelled_error());
                        }
                    }
                }
                execution = executions.next() => {
                    let Some(execution) = execution else {
                        continue;
                    };
                    if execution.dispatched {
                        dispatched = dispatched.saturating_add(1);
                    }
                    if execution.result.is_ok() {
                        let _ = command.shared.events.publish(
                            EventContext::call(
                                command.turn_id,
                                attempt_id,
                                execution.call.call_id(),
                            ),
                            AgentEventKind::ToolCompleted {
                                identity: execution.audit_identity.clone(),
                                lineage: execution.lineage.clone(),
                                observation: execution
                                    .result
                                    .as_ref()
                                    .ok()
                                    .map(|result| {
                                        tool_result_observation_with_subject(
                                            &result.result,
                                            result.presentation_subject.as_deref(),
                                        )
                                    }),
                            },
                        );
                    }
                    let index = execution.index;
                    completed[index] = Some(execution);
                }
                _ = job_changes.changed() => self.preempt_for_job_terminals(),
            }
        }
        if boundary_error.is_none()
            && (command.cancellation.is_cancelled() || self.shutdown.is_cancelled())
        {
            command.cancellation.cancel();
            boundary_error = Some(cancelled_error());
        }
        let completed = completed.into_iter().flatten().collect::<Vec<_>>();
        for execution in &completed {
            if let Some(update) = execution.process_session_update.clone() {
                self.apply_process_session_update(command.turn_id, update)
                    .await?;
            }
        }
        let transitions = completed
            .iter()
            .map(|execution| {
                tool_operation_terminal_state(
                    &execution.result,
                    execution.dispatched,
                    execution.result_entry_id,
                )
                .map(|state| (execution.operation_id, state))
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.transition_operations(command.turn_id, transitions)
            .await?;
        let mut indexed_errors = (0..items.len()).map(|_| None).collect::<Vec<_>>();
        for execution in completed {
            let index = execution.index;
            match execution.result {
                Err(error) => indexed_errors[index] = Some(error),
                Ok(result) => {
                    match canonicalize_observed_tool_execution(
                        execution.result_entry_id,
                        result,
                        &mut self.observation_budget,
                    ) {
                        Ok((item, clip)) => {
                            publish_observation_clip(
                                &command.shared.events,
                                command.turn_id,
                                attempt_id,
                                execution.call.call_id(),
                                execution.call.exposed_name(),
                                clip,
                            );
                            items[index] = Some(item);
                        }
                        Err(error) => indexed_errors[index] = Some(error),
                    }
                }
            }
        }
        let error = indexed_errors
            .into_iter()
            .flatten()
            .next()
            .or(boundary_error);
        if error.is_some() {
            command.cancellation.cancel();
        }
        error.map_or(Ok(dispatched), Err)
    }
}

impl Driver {
    pub(super) fn superseded_tool_execution(
        &self,
        call: &ToolCallBlock,
    ) -> Result<ToolExecutionResult, AgentError> {
        let context = self
            .active_root
            .as_ref()
            .map_or(self.telemetry_context, |root| {
                self.telemetry_context.with_turn(root.turn_id)
            })
            .with_call(call.call_id());
        self.emit_telemetry(
            context,
            crate::TelemetryObservationKind::JobPreempted {
                kind: crate::JobPreemption::Superseded,
            },
        );
        superseded_tool_execution(call)
    }
}
pub(super) fn superseded_tool_execution(
    call: &ToolCallBlock,
) -> Result<ToolExecutionResult, AgentError> {
    let internal = || {
        AgentError::new(
            AgentErrorKind::Internal,
            "superseded result could not be constructed",
        )
    };
    let text = TextBlock::new("not run: superseded by your message".to_owned(), None)
        .map_err(|_| internal())?;
    let result = ToolResultBlock::new(
        call.call_id(),
        call.owner().clone(),
        ToolResultKind::Failure,
        vec![ToolResultPart::Text(text)],
        call.correlation().clone(),
    )
    .map_err(|_| internal())?;
    Ok(ToolExecutionResult::new(result, false))
}

/// Batch-level inputs shared by every source-ordered call in one tool batch.
#[derive(Clone, Copy)]
struct ToolBatchInput<'a> {
    command: &'a PromptCommand,
    attempt_id: AttemptId,
    calls: &'a [ToolCallBlock],
    result_entry_ids: &'a [EntryId],
    tool_calls: usize,
    provider_operation: OperationId,
    provider_plan: &'a ValidatedRequestPlan,
    provider_response: &'a ModelResponse,
}

/// Source-ordered results, the pending read-only group and dispatch counts.
struct ToolBatchProgress {
    items: Vec<Option<ConversationItem>>,
    read_only: Vec<AdmittedToolCall>,
    dispatched: usize,
    operations: Vec<OperationId>,
}
