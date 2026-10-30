//! Read-only nested calls, started together and committed in call order.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the composition driver that shares its vocabulary"
)]

use super::*;

impl Driver {
    #[allow(
        clippy::too_many_lines,
        reason = "Nested read-only admission binds source-order slots and resource ownership before joining concurrent tool results"
    )]
    #[allow(
        clippy::result_large_err,
        reason = "the refusal hands the pending nested call back so the caller can settle it verbatim"
    )]
    pub(super) async fn start_local_composition_nested_read_only(
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
                    super::super::tools::AdmissionPresentation { presentation, want_value: state.integration().consumes_values() && pending.responder.wants_value() },
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
            let (notice_preempt, notice_guard) = self.nested_notice_preempt();
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
                preempt: notice_preempt,
                preempt_guard: Some(notice_guard),
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

    pub(super) async fn buffer_local_composition_nested_read_only(
        &mut self,
        command: &PromptCommand,
        state: &mut CompositionOwner,
        completion: LocalCompositionNestedReadOnlyCompletion,
    ) -> Result<(), AgentError> {
        state.buffer_read_only(completion)?;
        self.drain_local_composition_nested(command, state).await
    }

    pub(super) async fn buffer_local_composition_nested_rejection(
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
                execution_value = result.value.take();
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
                local_composition_nested_result_value(
                    &item,
                    state
                        .descriptor()
                        .nested_projection
                        .entries()
                        .iter()
                        .find(|entry| entry.exposed_name().as_str() == pending.call.tool_name),
                )
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
        crate::runtime::composition::nested::queue_local_composition_nested_values(
            state,
            pending.responder,
            waiters,
            result,
            execution_value,
        )
    }
}
