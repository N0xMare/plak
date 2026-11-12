use super::{
    AdmittedToolCall, AdmittedToolExecution, AgentError, AgentErrorKind, AttemptId,
    AuthorizationOutcome, AwaitEffectDecisionV1, AwaitServerPauseV1, AwaitToolBatchV2, CallId,
    ContinuationToolSlot, ConversationItem, Digest, Driver, DurableEffectIntentEnvelope,
    DurableOperationKind, EntryId, ModelRequest, OperationId, PendingEffectDecision, PromptCommand,
    RestoredHostedFrame, SessionStoreError, StoreAdmission, ToolAdmission, ToolAuditIdentity,
    ToolBatchContinuation, ToolBatchResult, ToolCallBlock, ToolDispatchOrigin, ToolExecutionResult,
    ToolInvocation, ToolProjectionReceiptV2, TurnResult, ValidatedRequestPlan,
    canonical_tool_operation_binding, canonicalize_observed_tool_execution, effect_intent_bytes,
    history_segment_kind, provider_artifact_transition, publish_observation_clip,
};

impl Driver {
    /// The plan a restored frame's provider response was validated under,
    /// checked against this driver's configuration through its stable
    /// identities only: a re-resolved or freshly reopened driver observes the
    /// descriptor afresh.
    pub(super) fn restore_frame_plan(
        &self,
        provider_operation: OperationId,
        provider_request: &ModelRequest,
        provider_plan_digest: Digest,
    ) -> Result<ValidatedRequestPlan, AgentError> {
        crate::runtime::continuation::restore_frame_plan(
            &self.provider_configuration,
            self.authority.checkpoint().manifest(),
            provider_operation,
            provider_request,
            provider_plan_digest,
        )
    }

    pub(super) async fn resume_restored_turn(
        &mut self,
        command: &PromptCommand,
        frame: AwaitEffectDecisionV1,
        invocation: ToolInvocation,
        projection_receipt: Option<ToolProjectionReceiptV2>,
        pending: PendingEffectDecision,
    ) -> Result<TurnResult, AgentError> {
        let provider_plan = self.restore_frame_plan(
            frame.provider_operation,
            &frame.provider_request,
            frame.provider_plan_digest,
        )?;
        let observation_budget_before_batch = self.observation_budget;
        let execution = Box::pin(self.resume_restored_batch(
            command,
            &frame,
            invocation,
            projection_receipt,
            pending,
            &provider_plan,
        ))
        .await;
        let execution = match execution {
            Ok(execution) => execution,
            Err(error) => {
                self.observation_budget = observation_budget_before_batch;
                let calls = frame
                    .slots
                    .iter()
                    .map(|slot| match slot {
                        ContinuationToolSlot::Completed { call, .. }
                        | ContinuationToolSlot::PendingDecision { call, .. }
                        | ContinuationToolSlot::NotAdmitted { call, .. } => call.clone(),
                    })
                    .collect::<Vec<_>>();
                let result_entry_ids = frame
                    .slots
                    .iter()
                    .map(|slot| match slot {
                        ContinuationToolSlot::Completed {
                            result_entry_id, ..
                        }
                        | ContinuationToolSlot::PendingDecision {
                            result_entry_id, ..
                        }
                        | ContinuationToolSlot::NotAdmitted {
                            result_entry_id, ..
                        } => *result_entry_id,
                    })
                    .collect::<Vec<_>>();
                return Err(Box::pin(self.close_interrupted_tool_batch(
                    command,
                    frame.attempt_id,
                    &calls,
                    &result_entry_ids,
                    frame.provider_operation,
                    &frame.provider_response,
                    Some(&frame.slots),
                    error,
                ))
                .await);
            }
        };
        let mut batch = frame.provider_response.output().to_vec();
        batch.extend(execution.items);
        let mut operations = execution.operations;
        operations.push(frame.provider_operation);
        let promoted = self
            .promote_tool_exchange_then_maybe_compact(
                command,
                frame.attempt_id,
                batch,
                operations,
                provider_artifact_transition(&provider_plan, &frame.provider_response),
            )
            .await;
        self.finalize_continuation_on_refusal(
            command.turn_id,
            frame.hosted_continuation_operation,
            promoted,
        )
        .await?;
        let _ = self.apply_steering(command).await?;
        command.shared.begin_preparing()?;
        let tool_calls = usize::try_from(frame.tool_calls)
            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?
            .saturating_add(execution.dispatched);
        self.run_turn_from_state(
            command,
            false,
            tool_calls,
            frame.hosted_continuation_operation,
        )
        .await
    }

    pub(super) async fn resume_hosted_server_pause(
        &mut self,
        command: &PromptCommand,
        restored: RestoredHostedFrame<AwaitServerPauseV1>,
    ) -> Result<TurnResult, AgentError> {
        self.hold_step_boundary(command).await?;
        let activity = self.boundary.0.activity(
            Some(command.turn_id),
            crate::control::boundary::BoundaryPhase::Effect,
        );
        let continuation_operation = restored.continuation_operation;
        let promoted = restored.promoted;
        let frame = restored.frame;
        let plan = self.restore_frame_plan(
            frame.provider_operation,
            &frame.provider_request,
            frame.provider_plan_digest,
        )?;
        if !promoted {
            self.validate_provider_artifact_candidate(&plan, &frame.provider_response)?;
            self.promote_completed_operations_with_history(
                command.turn_id,
                Some(history_segment_kind(frame.provider_response.output())),
                frame.provider_response.output().iter().cloned(),
                [frame.provider_operation],
                provider_artifact_transition(&plan, &frame.provider_response),
            )
            .await?;
        }
        let _ = self.apply_steering(command).await?;
        drop(activity);
        command.shared.continue_after_model()?;
        let tool_calls = usize::try_from(frame.tool_calls)
            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
        self.run_turn_from_state(command, false, tool_calls, Some(continuation_operation))
            .await
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Hosted tool-batch resume reconstructs completed slots and pending admission without replaying settled effects"
    )]
    pub(super) async fn resume_hosted_tool_batch(
        &mut self,
        command: &PromptCommand,
        restored: RestoredHostedFrame<AwaitToolBatchV2>,
    ) -> Result<TurnResult, AgentError> {
        self.hold_step_boundary(command).await?;
        let activity = self.boundary.0.activity(
            Some(command.turn_id),
            crate::control::boundary::BoundaryPhase::Effect,
        );
        let continuation_operation = restored.continuation_operation;
        let promoted = restored.promoted;
        let recovered_dispatched = restored.recovered_dispatched;
        self.observation_budget = restored.remaining_observation_budget;
        // A durable session canonicalizes its tool results in
        // `reconcile_hosted_tool_slots`, not at the driver's own call sites, so
        // this is the only place an ordinary durable round's clips can be
        // announced. Until 2026-09-18 they were computed there and discarded,
        // and the note never reached an operator on the path the product runs.
        //
        // This announces whenever the evidence is canonicalized, which on a
        // genuine reopen repeats a note the operator already saw. Telling the
        // two apart needs the frame to record that it has announced, and
        // repeating a true note is better than never showing it, so the repeat
        // stands until the frame can say otherwise.
        for clipped in &restored.observation_clips {
            publish_observation_clip(
                &command.shared,
                command.turn_id,
                restored.frame.attempt_id,
                clipped.call_id,
                &clipped.name,
                Some(clipped.clip),
            );
        }
        let frame = restored.frame;
        let plan = self.restore_frame_plan(
            frame.provider_operation,
            &frame.provider_request,
            frame.provider_plan_digest,
        )?;
        if !promoted {
            self.validate_provider_artifact_candidate(&plan, &frame.provider_response)?;
        }
        let calls = frame
            .slots
            .iter()
            .map(|slot| match slot {
                ContinuationToolSlot::Completed { call, .. }
                | ContinuationToolSlot::PendingDecision { call, .. }
                | ContinuationToolSlot::NotAdmitted { call, .. } => call.clone(),
            })
            .collect::<Vec<_>>();
        let result_entry_ids = frame
            .slots
            .iter()
            .map(|slot| match slot {
                ContinuationToolSlot::Completed {
                    result_entry_id, ..
                }
                | ContinuationToolSlot::PendingDecision {
                    result_entry_id, ..
                }
                | ContinuationToolSlot::NotAdmitted {
                    result_entry_id, ..
                } => *result_entry_id,
            })
            .collect::<Vec<_>>();
        if promoted {
            let _ = self.apply_steering(command).await?;
            command.shared.begin_preparing()?;
            let tool_calls = usize::try_from(frame.tool_calls)
                .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?
                .saturating_add(recovered_dispatched);
            return self
                .run_turn_from_state(command, false, tool_calls, Some(continuation_operation))
                .await;
        }
        let mut items = frame
            .slots
            .iter()
            .map(|slot| match slot {
                ContinuationToolSlot::Completed { item, .. } => Some(item.clone()),
                ContinuationToolSlot::PendingDecision { .. }
                | ContinuationToolSlot::NotAdmitted { .. } => None,
            })
            .collect::<Vec<_>>();
        let mut operations = frame
            .slots
            .iter()
            .filter_map(|slot| match slot {
                ContinuationToolSlot::Completed { operation_id, .. } => Some(*operation_id),
                _ => None,
            })
            .collect::<Vec<_>>();
        let prefix = operations.len();
        let mut dispatched = recovered_dispatched;
        let observation_budget_before_batch = self.observation_budget;
        let suffix = async {
            for index in prefix..calls.len() {
                let call = &calls[index];
                match self.admit_tool(command, frame.attempt_id, call).await? {
                    ToolAdmission::RejectedBeforeValidation { execution, .. }
                    | ToolAdmission::RejectedAfterValidation { execution } => {
                        let operation = self
                            .prepare_rejected_tool_operation(
                                command.turn_id,
                                frame.attempt_id,
                                u32::try_from(index).map_err(|_| {
                                    AgentError::from_store(SessionStoreError::corrupt())
                                })?,
                                call,
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
                            &command.shared,
                            command.turn_id,
                            frame.attempt_id,
                            call.call_id(),
                            call.exposed_name(),
                            clip,
                        );

                        items[index] = Some(item);
                        operations.push(operation);
                    }
                    ToolAdmission::Admitted { execution, .. } => {
                        let parent_call_id = self.active_composition_marker_call();
                        Box::pin(self.execute_restored_admitted(
                            command,
                            frame.attempt_id,
                            index,
                            call,
                            result_entry_ids[index],
                            *execution,
                            &mut items,
                            &mut operations,
                            &mut dispatched,
                            ToolDispatchOrigin::TopLevel,
                            parent_call_id,
                        ))
                        .await?;
                    }
                    ToolAdmission::DecisionRequired { .. } => {
                        return Err(AgentError::new(
                            AgentErrorKind::OutcomeUnknown,
                            "hosted batch reopen reached an unpersisted effect decision",
                        ));
                    }
                }
            }
            Ok::<(), AgentError>(())
        }
        .await;
        if let Err(error) = suffix {
            self.observation_budget = observation_budget_before_batch;
            return Err(Box::pin(self.close_interrupted_tool_batch(
                command,
                frame.attempt_id,
                &calls,
                &result_entry_ids,
                frame.provider_operation,
                &frame.provider_response,
                Some(&frame.slots),
                error,
            ))
            .await);
        }
        let mut batch = frame.provider_response.output().to_vec();
        batch.extend(
            items
                .into_iter()
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?,
        );
        operations.push(frame.provider_operation);
        let promoted = self
            .promote_tool_exchange_then_maybe_compact(
                command,
                frame.attempt_id,
                batch,
                operations,
                provider_artifact_transition(&plan, &frame.provider_response),
            )
            .await;
        self.finalize_continuation_on_refusal(
            command.turn_id,
            Some(continuation_operation),
            promoted,
        )
        .await?;
        let _ = self.apply_steering(command).await?;
        command.shared.begin_preparing()?;
        let tool_calls = usize::try_from(frame.tool_calls)
            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?
            .saturating_add(dispatched);
        drop(activity);
        self.run_turn_from_state(command, false, tool_calls, Some(continuation_operation))
            .await
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Restored-batch execution preserves source order while reconciling completed, undecided, and undispatched slots"
    )]
    pub(super) async fn resume_restored_batch(
        &mut self,
        command: &PromptCommand,
        frame: &AwaitEffectDecisionV1,
        invocation: ToolInvocation,
        projection_receipt: Option<ToolProjectionReceiptV2>,
        pending: PendingEffectDecision,
        provider_plan: &ValidatedRequestPlan,
    ) -> Result<ToolBatchResult, AgentError> {
        let parked = self.hold_step_boundary(command).await?;
        let mut frame = frame.clone();
        frame.decision_expires_at =
            crate::runtime::config::timestamp_after(frame.decision_expires_at, parked);
        let _activity = self.boundary.0.activity(
            Some(command.turn_id),
            crate::control::boundary::BoundaryPhase::Effect,
        );
        let pending_index = usize::try_from(frame.pending_index)
            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
        let calls = frame
            .slots
            .iter()
            .map(|slot| match slot {
                ContinuationToolSlot::Completed { call, .. }
                | ContinuationToolSlot::PendingDecision { call, .. }
                | ContinuationToolSlot::NotAdmitted { call, .. } => call.clone(),
            })
            .collect::<Vec<_>>();
        let result_entry_ids = frame
            .slots
            .iter()
            .map(|slot| match slot {
                ContinuationToolSlot::Completed {
                    result_entry_id, ..
                }
                | ContinuationToolSlot::PendingDecision {
                    result_entry_id, ..
                }
                | ContinuationToolSlot::NotAdmitted {
                    result_entry_id, ..
                } => *result_entry_id,
            })
            .collect::<Vec<_>>();
        let mut items = frame
            .slots
            .iter()
            .map(|slot| match slot {
                ContinuationToolSlot::Completed { item, .. } => Some(item.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut operations = frame
            .slots
            .iter()
            .take(pending_index)
            .filter_map(|slot| match slot {
                ContinuationToolSlot::Completed { operation_id, .. } => Some(*operation_id),
                _ => None,
            })
            .collect::<Vec<_>>();
        let pending_call = &calls[pending_index];
        let authorization =
            Box::pin(self.drive_restored_effect_decision(command, pending_call, pending, &frame))
                .await?;
        let mut dispatched = 0usize;
        Box::pin(self.execute_restored_authorization(
            command,
            frame.attempt_id,
            pending_index,
            pending_call,
            result_entry_ids[pending_index],
            invocation,
            projection_receipt,
            authorization,
            &mut items,
            &mut operations,
            &mut dispatched,
        ))
        .await?;
        for index in (pending_index + 1)..calls.len() {
            let call = &calls[index];
            let admission = self.admit_tool(command, frame.attempt_id, call).await?;
            match admission {
                ToolAdmission::RejectedBeforeValidation { execution, .. }
                | ToolAdmission::RejectedAfterValidation { execution } => {
                    let operation = self
                        .prepare_rejected_tool_operation(
                            command.turn_id,
                            frame.attempt_id,
                            u32::try_from(index).map_err(|_| {
                                AgentError::from_store(SessionStoreError::corrupt())
                            })?,
                            call,
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
                        &command.shared,
                        command.turn_id,
                        frame.attempt_id,
                        call.call_id(),
                        call.exposed_name(),
                        clip,
                    );

                    items[index] = Some(item);
                    operations.push(operation);
                }
                ToolAdmission::Admitted { execution, .. } => {
                    let parent_call_id = self.active_composition_marker_call();
                    Box::pin(self.execute_restored_admitted(
                        command,
                        frame.attempt_id,
                        index,
                        call,
                        result_entry_ids[index],
                        *execution,
                        &mut items,
                        &mut operations,
                        &mut dispatched,
                        ToolDispatchOrigin::TopLevel,
                        parent_call_id,
                    ))
                    .await?;
                }
                ToolAdmission::DecisionRequired {
                    invocation,
                    projection_receipt,
                    pending,
                    ..
                } => {
                    let continuation = ToolBatchContinuation {
                        tool_calls: usize::try_from(frame.tool_calls)
                            .unwrap_or(usize::MAX)
                            .saturating_add(dispatched),
                        provider_operation: frame.provider_operation,
                        provider_plan,
                        provider_response: &frame.provider_response,
                        calls: &calls,
                        result_entry_ids: &result_entry_ids,
                        items: &items,
                        operations: &operations,
                        source_index: index,
                    };
                    let authorization = Box::pin(self.await_effect_decision(
                        command,
                        frame.attempt_id,
                        call,
                        *pending,
                        &continuation,
                    ))
                    .await?;
                    Box::pin(self.execute_restored_authorization(
                        command,
                        frame.attempt_id,
                        index,
                        call,
                        result_entry_ids[index],
                        *invocation,
                        projection_receipt,
                        authorization,
                        &mut items,
                        &mut operations,
                        &mut dispatched,
                    ))
                    .await?;
                }
            }
        }
        Ok(ToolBatchResult {
            items: items
                .into_iter()
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "restored tool suffix remained incomplete",
                    )
                })?,
            dispatched,
            operations,
        })
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "restored authorization preserves the complete persisted source slot"
    )]
    pub(super) async fn execute_restored_authorization(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        index: usize,
        call: &ToolCallBlock,
        result_entry_id: EntryId,
        invocation: ToolInvocation,
        projection_receipt: Option<ToolProjectionReceiptV2>,
        authorization: AuthorizationOutcome,
        items: &mut [Option<ConversationItem>],
        operations: &mut Vec<OperationId>,
        dispatched: &mut usize,
    ) -> Result<(), AgentError> {
        let authorization = if *self.tool_preempt.borrow() == plakous_tools::ToolPreempt::Steer
            && matches!(authorization, AuthorizationOutcome::Authorized(_))
        {
            AuthorizationOutcome::Rejected(self.superseded_tool_execution(call)?.result)
        } else {
            authorization
        };
        match authorization {
            AuthorizationOutcome::Rejected(result) => {
                let execution = ToolExecutionResult::new(result, false);
                let operation = self
                    .prepare_rejected_tool_operation(
                        command.turn_id,
                        attempt_id,
                        u32::try_from(index)
                            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?,
                        call,
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
                    &command.shared,
                    command.turn_id,
                    attempt_id,
                    call.call_id(),
                    call.exposed_name(),
                    clip,
                );

                items[index] = Some(item);
                operations.push(operation);
                Ok(())
            }
            AuthorizationOutcome::Authorized(authorized) => {
                let admitted =
                    self.admitted_tool_execution(invocation, projection_receipt, *authorized)?;
                let parent_call_id = self.active_composition_marker_call();
                Box::pin(self.execute_restored_admitted(
                    command,
                    attempt_id,
                    index,
                    call,
                    result_entry_id,
                    admitted,
                    items,
                    operations,
                    dispatched,
                    ToolDispatchOrigin::TopLevel,
                    parent_call_id,
                ))
                .await
                .map(|_| ())
            }
            AuthorizationOutcome::Pending(_) => Err(AgentError::new(
                AgentErrorKind::Internal,
                "resolved restored decision remained pending",
            )),
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Restored authorized execution uses the recorded one-shot grant and publishes its exact effect outcome"
    )]
    #[allow(
        clippy::too_many_arguments,
        reason = "restored dispatch preserves the complete persisted operation tuple"
    )]
    pub(in crate::runtime::driver) async fn execute_restored_admitted(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        index: usize,
        call: &ToolCallBlock,
        result_entry_id: EntryId,
        execution: AdmittedToolExecution,
        items: &mut [Option<ConversationItem>],
        operations: &mut Vec<OperationId>,
        dispatched: &mut usize,
        origin: ToolDispatchOrigin,
        parent_call_id: Option<CallId>,
    ) -> Result<Option<plakous_tools::ExecutionValue>, AgentError> {
        let mut value = None;
        let source_index = u32::try_from(index)
            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
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
                let binding = canonical_tool_operation_binding(index, &invocation)?;
                let input = serde_json::to_vec(&(binding, call)).map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "restored read-only operation input could not be captured",
                    )
                })?;
                let operation_id = self
                    .prepare_operation(
                        StoreAdmission::Ordinary,
                        command.turn_id,
                        kind,
                        binding,
                        Some(input),
                        None,
                    )
                    .await?;
                let audit_identity = ToolAuditIdentity::from_invocation(&invocation);
                let mut one = vec![None; items.len()];
                *dispatched = dispatched.saturating_add(
                    Box::pin(self.execute_read_only_group(
                        command,
                        attempt_id,
                        vec![AdmittedToolCall {
                            operation_id,
                            index,
                            call: call.clone(),
                            result_entry_id,
                            audit_identity,
                            execution: AdmittedToolExecution::ReadOnly {
                                invocation,
                                projection_receipt,
                                authorized,
                                effect,
                            },
                        }],
                        &mut one,
                        origin,
                        parent_call_id,
                    ))
                    .await?,
                );
                items[index] = one[index].take();
                operations.push(operation_id);
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
                        "restored mutating intent could not be captured",
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
                    .claim_mutating_effect(command.turn_id, operation_id, *intent, *authorized)
                    .await?;
                let audit_identity = ToolAuditIdentity::from_invocation(&invocation);
                // Boxed: a nested composition child reaches this frame through
                // the cell driver and the effect loop, and a debug build's
                // unboxed poll depth otherwise exhausts a 2 MiB thread stack.
                let execution = Box::pin(self.execute_mutating_effect(
                    command,
                    attempt_id,
                    operation_id,
                    *invocation,
                    projection_receipt,
                    claimed,
                    call,
                    &audit_identity,
                    result_entry_id,
                    origin,
                    parent_call_id,
                ))
                .await?;
                *dispatched = dispatched.saturating_add(usize::from(execution.dispatched));
                items[index] = Some(execution.item);
                value = execution.value;
                operations.push(operation_id);
            }
        }
        Ok(value)
    }
}
