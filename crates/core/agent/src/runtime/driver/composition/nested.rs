//! Nested tool calls a cell makes, dispatched one at a time.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the composition driver that shares its vocabulary"
)]

use super::*;

impl Driver {
    /// Commits inside a running cell are settlement, so a nested call is the
    /// cell's new work and meets the ordinary threshold here, before anything
    /// is journaled. The refusal is a definite error the cell can catch.
    pub(super) async fn refuse_nested_under_store_pressure(&self) -> Result<(), AgentError> {
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

    pub(super) async fn dispatch_next_local_composition_nested(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        state: &mut CompositionOwner,
    ) -> Result<(), AgentError> {
        let Some(pending) = state.take_serial_pending()? else {
            return Ok(());
        };
        let runtime_call_id = pending.call.runtime_call_id.clone();
        let result = Box::pin(self.execute_local_composition_nested(
            command,
            attempt_id,
            state,
            pending.call,
            pending.request_digest,
            pending.request_sequence,
        ))
        .await;
        state.finish_serial(&runtime_call_id, &result)?;
        let waiters = take_local_composition_nested_waiters(state, &runtime_call_id);
        queue_local_composition_nested_results(state, pending.responder, waiters, result)
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
        let value = local_composition_nested_result_value(
            item,
            state
                .descriptor()
                .nested_projection
                .entries()
                .iter()
                .find(|entry| entry.exposed_name() == call.exposed_name()),
        );
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
