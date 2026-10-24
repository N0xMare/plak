//! The outer composition call: exec and wait, each driving one cell to its next yield or end.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the composition driver that shares its vocabulary"
)]

use super::*;

impl Driver {
    pub(in crate::runtime::driver) async fn execute_local_composition_outer(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        source_index: u32,
        call: &ToolCallBlock,
        result_entry_id: EntryId,
        execution: AdmittedToolExecution,
    ) -> Result<LocalCompositionOuterExecution, AgentError> {
        if (*self.tool_preempt.borrow()).is_notice() {
            self.tool_preempt
                .send_replace(plakous_tools::ToolPreempt::None);
        }
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
        let result = local_composition_response_parts(
            &response,
            max_output_tokens,
            wall_time,
            integration.integration().as_ref(),
        )
        .and_then(|parts| {
            ToolOutcome::success(parts).map_err(|error| AgentError::from_tool(&error))
        })
        .and_then(|outcome| {
            self.tool_presentation
                .raise(projection_receipt.as_ref(), outcome)
                .and_then(|outcome| outcome.to_model_result(&invocation))
                .map_err(|error| AgentError::from_tool(&error))
        })
        .or_else(|error| {
            let parts = local_composition_refusal_parts(
                &response,
                wall_time,
                integration.integration().result_projection(),
                &error,
            )?;
            // Presentation itself may have refused the result. Its fallback
            // is already model-visible text, not another raising attempt.
            ToolOutcome::success(parts)
                .and_then(|outcome| outcome.to_model_result(&invocation))
                .map_err(|error| AgentError::from_tool(&error))
        })?;
        // Recovery restores the pre-batch observation budget and cuts once.
        // Retain the bounded, presented parts, never an already-cut view.
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
                result.parts(),
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
        let mut job_changes = self.job_sessions.job_terminal_changes();
        job_changes.mark_changed();
        let result: Result<LocalCompositionResponse, AgentError> = loop {
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
}
