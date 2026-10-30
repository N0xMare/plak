use super::{
    Actor, AgentError, AgentErrorKind, AgentEventKind, ArtifactTransition, AttemptId, AttemptRetry,
    AwaitToolBatchV2, CommitPlan, ContentBlock, ContinuationBindingReference, ContinuationRoot,
    ContinuationSteering, ContinuationToolSlot, ConversationItem, Driver, DurableAgentConfig,
    DurableOperationKind, DurableOperationRecord, DurableOperationState,
    DurableOperationTransition, EntryId, EventContext, HistorySegmentKind, HostedRoundDisposition,
    ModelError, ModelErrorKind, ModelErrorPhase, ModelResponse, OPERATION_BUDGET_WRAP_UP_NOTE,
    OperationId, PromptCommand, Provenance, ProviderOutcome, RoundClassificationError,
    RoundClassificationInput, RoundDecision, RoundRequest, RuntimeComponentKind,
    RuntimeContinuation, SessionStoreError, SettledRound, TextBlock, ToolCallBlock, TurnId,
    TurnResult, Usage, ValidatedRequestPlan, WorkingProjection,
    apply_completed_model_round_occupancy, cancelled_error, classify_round, operation_turn_id,
    output_limit_retry_eligible, output_limit_retry_note, provider_artifact_transition,
    resolve_model_operation, response_segment, round_failure_action, runtime_component,
    runtime_manifest_error,
};

impl Driver {
    /// A `Continue` round in one commit: the provider's outcome record and the
    /// response's history, finalized together.
    pub(super) async fn commit_continue_round(
        &mut self,
        turn_id: TurnId,
        provider_operation: OperationId,
        response: &ModelResponse,
        usage: Option<Usage>,
        continuation_baton: Option<OperationId>,
        artifact: ArtifactTransition,
    ) -> Result<(), AgentError> {
        let outcome = self.provider_outcome(Ok(response), usage, RoundDecision::Continue)?;
        let mut plan = self.commit_plan()?;
        self.stage_provider_outcome(
            &mut plan,
            provider_operation,
            Ok(response),
            &outcome,
            continuation_baton,
        )?;
        drop(outcome);
        plan.finalize.insert(provider_operation);
        plan.segments.extend(response_segment(response)?);
        plan.artifact = artifact;
        self.publish_commit_plan(turn_id, plan).await?;
        Ok(())
    }

    /// A hosted round's decision with its restorable frame in one commit, and
    /// for a server pause the response's history too.
    #[allow(
        clippy::too_many_arguments,
        reason = "the frame binds the exact provider, its outcome, prior baton and continuation"
    )]
    pub(super) async fn commit_hosted_round(
        &mut self,
        turn_id: TurnId,
        provider_operation: OperationId,
        outcome: ProviderOutcome,
        prior_continuation: Option<OperationId>,
        kind: DurableOperationKind,
        continuation: RuntimeContinuation,
        history: Option<(&ModelResponse, ArtifactTransition)>,
    ) -> Result<OperationId, AgentError> {
        let mut plan = self.commit_plan()?;
        let operation = self.stage_hosted_continuation(
            &mut plan,
            turn_id,
            provider_operation,
            &outcome,
            prior_continuation,
            kind,
            &continuation,
        )?;
        drop((outcome, continuation));
        if let Some((response, artifact)) = history {
            plan.finalize.insert(provider_operation);
            plan.segments.extend(response_segment(response)?);
            plan.artifact = artifact;
        }
        self.publish_commit_plan(turn_id, plan).await?;
        Ok(operation)
    }

    /// Adds a hosted round's decision and its restorable frame to `plan`: the
    /// provider's outcome record, the finalization of the frame it resumed
    /// from, and the new continuation record.
    #[allow(
        clippy::too_many_arguments,
        reason = "the frame binds the exact provider, its outcome, prior baton and continuation"
    )]
    pub(super) fn stage_hosted_continuation(
        &mut self,
        plan: &mut CommitPlan,
        turn_id: TurnId,
        provider_operation: OperationId,
        provider_outcome: &ProviderOutcome,
        prior_continuation: Option<OperationId>,
        kind: DurableOperationKind,
        continuation: &RuntimeContinuation,
    ) -> Result<OperationId, AgentError> {
        let bytes = serde_json::to_vec(continuation).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "hosted round continuation could not be canonically encoded",
            )
        })?;
        let component = runtime_component(RuntimeComponentKind::Operation, bytes)?;
        let operation = self.take_operation_id()?;
        let authority = self.authority.checkpoint();
        let ledger = authority.manifest().ledger();
        let provider = ledger
            .operation(provider_operation.0)
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
        if operation_turn_id(provider) != Some(turn_id)
            || !matches!(
                provider.state(),
                DurableOperationState::Dispatched | DurableOperationState::LocatorBound(_)
            )
        {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
        plan.transitions.push((
            provider_operation,
            DurableOperationState::Completed(provider_outcome.component()?),
        ));
        if let Some(prior_continuation) = prior_continuation {
            let prior = ledger
                .operation(prior_continuation.0)
                .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
            if !matches!(
                prior.state(),
                DurableOperationState::ContinuationRequired(_)
            ) {
                return Err(AgentError::from_store(SessionStoreError::corrupt()));
            }
            plan.finalize.insert(prior_continuation);
        }
        let prior_continuations = ledger
            .operations()
            .into_iter()
            .filter(|record| {
                Some(OperationId(record.operation_id())) != prior_continuation
                    && operation_turn_id(record) == Some(turn_id)
                    && matches!(
                        record.kind(),
                        DurableOperationKind::ToolBatchContinuation { .. }
                            | DurableOperationKind::ServerPauseContinuation { .. }
                    )
                    && matches!(
                        record.state(),
                        DurableOperationState::ContinuationRequired(_)
                    )
            })
            .map(|record| (record.operation_id(), record.request().clone()))
            .collect::<Vec<_>>();
        if prior_continuations.len() > 1 {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
        for (prior_operation, evidence) in prior_continuations {
            plan.transitions.push((
                OperationId(prior_operation),
                DurableOperationState::Finalized(evidence),
            ));
        }
        let record = DurableOperationRecord::new(
            operation.0,
            kind,
            component.clone(),
            vec![DurableOperationTransition::new(
                plan.checkpoint_id,
                DurableOperationState::ContinuationRequired(component),
            )],
        )
        .map_err(runtime_manifest_error)?;
        plan.appended.push(record);
        Ok(operation)
    }

    /// Appends a harness note to the active turn and publishes the event that
    /// tells a surface why it was injected.
    ///
    /// The note is a conversation segment, never recent intent: only the
    /// prompt that opened the turn is, so the note neither opens a root turn
    /// (spec 01 §10.6) nor closes the active root's history.
    ///
    /// Canonical history is what the model reads, not what a surface reads:
    /// without the event the user would see the model change course with no
    /// visible reason.
    pub(super) async fn append_harness_note(
        &mut self,
        command: &PromptCommand,
        note: String,
        event: AgentEventKind,
    ) -> Result<(), AgentError> {
        let item = self.harness_note(command, note)?;
        self.append_history(
            command.turn_id,
            Some(HistorySegmentKind::Conversation),
            [item],
        )
        .await?;
        let _ = command
            .shared
            .events
            .publish(EventContext::turn(command.turn_id), event);
        Ok(())
    }

    pub(super) fn harness_note(
        &self,
        command: &PromptCommand,
        note: String,
    ) -> Result<ConversationItem, AgentError> {
        ConversationItem::new(
            self.entry_id()?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new(note, None).map_err(
                |_| AgentError::new(AgentErrorKind::Internal, "harness note text is invalid"),
            )?)],
            if command.instruction {
                Provenance::Instruction
            } else {
                Provenance::UserInput
            },
        )
        .map_err(|_| AgentError::new(AgentErrorKind::Internal, "harness note item is invalid"))
    }

    /// Commits a `RetryOutputLimit` decision with its note.
    ///
    /// A truncated call never runs, so nothing is replayed: the model is told
    /// what happened and asked again.
    pub(super) async fn commit_output_limit_retry(
        &mut self,
        command: &PromptCommand,
        provider_operation: Option<OperationId>,
        attempt_error: Option<&AgentError>,
        usage: Option<Usage>,
        continuation_baton: Option<OperationId>,
        error: &AgentError,
    ) -> Result<(), AgentError> {
        let bound = error
            .model_error()
            .and_then(ModelError::wire_output_limit)
            .or_else(|| self.stable_request_prefix.limits().max_output_tokens());
        let note = self.harness_note(command, output_limit_retry_note(bound))?;
        self.commit_retry(
            command.turn_id,
            provider_operation,
            attempt_error,
            usage,
            RoundDecision::RetryOutputLimit,
            continuation_baton,
            note,
            HistorySegmentKind::Conversation,
        )
        .await?;
        let _ = command.shared.events.publish(
            EventContext::turn(command.turn_id),
            AgentEventKind::OutputLimitRetry { bound },
        );
        command.shared.continue_after_model()?;
        Ok(())
    }

    pub(super) fn continuation_root(command: &PromptCommand) -> ContinuationRoot {
        ContinuationRoot {
            command_id: command.command_id.as_str().to_owned(),
            turn_id: command.turn_id,
            follow_up: command.follow_up,
            operation_id: command.operation_id,
            binding: command.binding,
            max_model_rounds: command.max_model_rounds,
        }
    }

    pub(super) fn continuation_bindings(
        &self,
    ) -> (ContinuationBindingReference, ContinuationBindingReference) {
        let authority = self.authority.checkpoint();
        let manifest = authority.manifest();
        (
            ContinuationBindingReference::from(manifest.binding().admission().reference()),
            ContinuationBindingReference::from(manifest.tools().policy()),
        )
    }

    pub(super) async fn run_turn_from_state(
        &mut self,
        command: &PromptCommand,
        append_user: bool,
        tool_calls: usize,
        continuation_baton: Option<OperationId>,
    ) -> Result<TurnResult, AgentError> {
        let result = self
            .run_turn_rounds(command, append_user, tool_calls, continuation_baton)
            .await;
        // A retained preparation of this turn's next round never outlives the
        // turn, however the turn ends.
        self.admitted_round = None;
        result
    }

    #[allow(
        clippy::too_many_lines,
        reason = "The root-turn round loop keeps model decisions, tool batches, steering, and budget terminalization under one owner"
    )]
    pub(super) async fn run_turn_rounds(
        &mut self,
        command: &PromptCommand,
        append_user: bool,
        mut tool_calls: usize,
        mut continuation_baton: Option<OperationId>,
    ) -> Result<TurnResult, AgentError> {
        if command.cancellation.is_cancelled() || self.shutdown.is_cancelled() {
            return Err(cancelled_error());
        }
        self.appended_prompt = None;
        if append_user {
            self.append_turn_prompt(command).await?;
        }
        // A resumed continuation's prompt was appended by an earlier frame.
        self.prompt_in_history = Some(command.turn_id);
        // Derived, never simply `false`: a resumed continuation restores its
        // round count but not this frame's locals, and round `max + 1` is only
        // ever the wrap-up round. Entering above the cap therefore means the
        // wrap-up already ran, and spec 01 §3.3 allows exactly one.
        let mut wrap_up_round_taken = self.turn_budgets.over_round_cap(command.max_model_rounds);
        let mut wrap_up_note_appended = wrap_up_round_taken;
        // The prompt append checked the segment limit just before this frame's
        // first round.
        let mut segment_limit_checked = append_user;
        loop {
            if command.cancellation.is_cancelled() || self.shutdown.is_cancelled() {
                command.cancellation.cancel();
                return Err(cancelled_error());
            }
            if !std::mem::take(&mut segment_limit_checked)
                && let Some(refusal) = self.segment_limit_refusal(command.turn_id)
            {
                self.finalize_hosted_continuation(command.turn_id, continuation_baton)
                    .await?;
                return Err(refusal);
            }
            self.reresolve_expired_model(command).await?;
            let residency = self.require_image_residency(command).await;
            self.finalize_continuation_on_request_budget(
                command.turn_id,
                continuation_baton,
                residency,
            )
            .await?;
            self.turn_budgets.model_rounds = self.turn_budgets.model_rounds.saturating_add(1);
            // The budget ends exploration, not the turn. One wrap-up round runs
            // with tool selection withdrawn so the user still gets an answer;
            // only a model that keeps asking for tools anyway fails the turn.
            let over_budget = self.turn_budgets.over_round_cap(command.max_model_rounds);
            if over_budget {
                if wrap_up_round_taken {
                    self.finalize_hosted_continuation(command.turn_id, continuation_baton)
                        .await?;
                    return Err(AgentError::new(
                        AgentErrorKind::RoundLimit,
                        "logical turn exceeded its model-operation limit",
                    ));
                }
                wrap_up_round_taken = true;
                if !wrap_up_note_appended {
                    self.append_harness_note(
                        command,
                        OPERATION_BUDGET_WRAP_UP_NOTE.to_owned(),
                        AgentEventKind::OperationBudgetExhausted,
                    )
                    .await?;
                    wrap_up_note_appended = true;
                }
            }
            let (attempt_id, request) =
                if let Some(admitted) = self.take_admitted_round(command.turn_id, over_budget)? {
                    (admitted.attempt_id, RoundRequest::Admitted(admitted))
                } else {
                    let attempt_id = self.attempt_id()?;
                    let request = self.model_request(command, attempt_id, over_budget)?;
                    (attempt_id, RoundRequest::Fresh(Box::new(request)))
                };
            let operation = self.run_model(command, attempt_id, request).await;
            let last_round_usage = operation.usage;
            let usage_result = command.shared.record_model_operation(last_round_usage);
            let provider_operation = operation.operation_id;
            let provider_plan = operation.plan;
            let wire_output_limit = operation.wire_output_limit;
            let attempt_error = operation.result.as_ref().err().cloned();
            let response = match resolve_model_operation(operation.result, usage_result) {
                Ok(response) => response,
                // The attempt ended with this definite error and ran nothing.
                // Its decision commits first; then the turn acts on it.
                Err(error) => {
                    match Box::pin(self.settle_failed_attempt(
                        command,
                        error,
                        over_budget,
                        continuation_baton,
                        provider_operation,
                        attempt_error,
                        last_round_usage,
                    ))
                    .await?
                    {
                        AttemptRetry::ContentFilter => {
                            continuation_baton = None;
                            if over_budget {
                                wrap_up_round_taken = false;
                            }
                        }
                        AttemptRetry::OutputLimit => continuation_baton = None,
                        AttemptRetry::OverflowFold => {}
                    }
                    continue;
                }
            };
            let provider_operation = provider_operation.ok_or_else(|| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "completed provider response lost its durable operation identity",
                )
            })?;
            let provider_plan = provider_plan.ok_or_else(|| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "completed provider response lost its validated request plan",
                )
            })?;
            if let Err(error) = self.validate_provider_artifact_candidate(&provider_plan, &response)
            {
                let decision = self.fail_turn_decision(command.turn_id, &error).await?;
                self.record_provider_decision(
                    command.turn_id,
                    provider_operation,
                    Err(&error),
                    last_round_usage,
                    decision,
                    continuation_baton,
                )
                .await?;
                return Err(error);
            }
            if let Some(candidate) = response.family_artifact_candidate() {
                self.artifact_producer_prefix = Some((
                    candidate.binding(),
                    provider_plan.request().stable_prefix().clone(),
                ));
            }
            let classification = classify_round(RoundClassificationInput {
                response: &response,
                history: &self.conversation(),
                over_budget,
                wire_output_limit,
            });
            let _ = command.shared.events.publish(
                EventContext::attempt(command.turn_id, attempt_id),
                AgentEventKind::ResponseSettled {
                    discardable: response.finish().reason() == plakous_model::FinishReason::Length
                        && classification.is_err(),
                },
            );
            let disposition = match classification {
                Ok(disposition) => disposition,
                Err(classification_error) => {
                    let action = round_failure_action(&classification_error);
                    let error = match classification_error {
                        RoundClassificationError::InvalidCalls(error)
                        | RoundClassificationError::InvalidDisposition(error)
                        | RoundClassificationError::IncompleteOutput(error)
                        | RoundClassificationError::NonRetryableOutput(error) => {
                            self.project_model_error(error, action.phase)
                        }
                        RoundClassificationError::RoundLimit => AgentError::new(
                            AgentErrorKind::RoundLimit,
                            "logical turn exceeded its model-operation limit",
                        ),
                    };
                    let retry = action.retry_output_limit
                        && output_limit_retry_eligible(
                            error.model_kind(),
                            self.turn_budgets.output_limit_retry_taken,
                            over_budget,
                        );
                    if !retry {
                        let decision = self.fail_turn_decision(command.turn_id, &error).await?;
                        // Finishing the provider also finalizes the continuation baton once.
                        self.record_provider_decision(
                            command.turn_id,
                            provider_operation,
                            Err(&error),
                            last_round_usage,
                            decision,
                            continuation_baton,
                        )
                        .await?;
                        return Err(error);
                    }
                    self.turn_budgets.output_limit_retry_taken = true;
                    Box::pin(self.commit_output_limit_retry(
                        command,
                        Some(provider_operation),
                        Some(&error),
                        last_round_usage,
                        continuation_baton,
                        &error,
                    ))
                    .await?;
                    continuation_baton = None;
                    continue;
                }
            };
            // One retry per truncation, reset by a round that classified: a
            // model that overruns its bound again right after being told
            // fails the turn with the output-limit error.
            self.turn_budgets.output_limit_retry_taken = false;
            let descriptor = self.provider_configuration.effective().descriptor();
            let same_served_model = response
                .metadata()
                .actual_model()
                .is_none_or(|model| descriptor.serves_same_model(model));
            if let Some(usage) = response.usage().filter(|_| same_served_model) {
                apply_completed_model_round_occupancy(
                    &mut self.context_anchor,
                    &mut self.nested_committed_observation_tokens,
                    (usage.input_tokens() > 0)
                        .then_some(usage.input_tokens().saturating_add(usage.output_tokens())),
                    operation
                        .request_estimate_tokens
                        .map(|estimate| estimate.saturating_add(usage.output_tokens())),
                );
                let descriptor = self.provider_configuration.effective().descriptor();
                if let Some(input) = operation.request_wire_input
                    && let Ok(binding) = descriptor.semantic_binding_digest()
                    && let Some(anchor) = plakous_model::ContextRequestAnchor::new(
                        binding,
                        descriptor.route().clone(),
                        response
                            .metadata()
                            .actual_model()
                            .unwrap_or_else(|| descriptor.requested_model())
                            .clone(),
                        usage,
                        input,
                    )
                {
                    if let Some(headroom) = operation
                        .request_context_status
                        .as_ref()
                        .and_then(|status| status.completed_response_headroom(&anchor))
                    {
                        self.observation_budget = crate::runtime::observation::budget::ObservationBudget::new(
                            crate::runtime::observation::budget::observation_bytes_from_headroom(headroom),
                            self.config.observation_budget_cap_bytes,
                        );
                    }
                    self.request_anchor = Some(anchor);
                }
            }
            if !matches!(
                &disposition,
                HostedRoundDisposition::CustomerToolsRequested(_)
            ) {
                match Box::pin(self.settle_model_round(
                    command,
                    &disposition,
                    tool_calls,
                    attempt_id,
                    provider_operation,
                    &provider_plan,
                    response,
                    last_round_usage,
                    continuation_baton,
                ))
                .await?
                {
                    SettledRound::Continue(baton) => {
                        continuation_baton = baton;
                        continue;
                    }
                    SettledRound::Finished(result) => return Ok(*result),
                }
            }
            let HostedRoundDisposition::CustomerToolsRequested(calls) = disposition else {
                return Err(AgentError::new(
                    AgentErrorKind::Internal,
                    "hosted round classification lost its customer-tool batch",
                ));
            };

            let result_entry_ids = (0..calls.len())
                .map(|_| self.entry_id())
                .collect::<Result<Vec<_>, _>>()?;
            let provider_programmatic_count = calls
                .iter()
                .filter(|call| {
                    call.correlation()
                        .caller_lineage()
                        .provider_programmatic_kind()
                        .is_some()
                })
                .count();
            let provider_programmatic = provider_programmatic_count != 0;
            if provider_programmatic && response.family_artifact_candidate().is_none() {
                let error = self.model_failure(
                    ModelErrorKind::IncompleteResponse,
                    "provider-programmatic batch omitted its exact family artifact",
                    ModelErrorPhase::Terminal,
                );
                let decision = self.fail_turn_decision(command.turn_id, &error).await?;
                self.record_provider_decision(
                    command.turn_id,
                    provider_operation,
                    Err(&error),
                    last_round_usage,
                    decision,
                    continuation_baton,
                )
                .await?;
                return Err(error);
            }
            if !provider_programmatic {
                self.record_provider_decision(
                    command.turn_id,
                    provider_operation,
                    Ok(&response),
                    last_round_usage,
                    RoundDecision::ToolBatch,
                    continuation_baton,
                )
                .await?;
                continuation_baton = None;
            }
            let continuation = self.tool_batch_continuation(
                command,
                tool_calls,
                attempt_id,
                provider_operation,
                &provider_plan,
                &response,
                &calls,
                &result_entry_ids,
            )?;
            let continuation_operation = if provider_programmatic {
                let outcome = self.provider_outcome(
                    Ok(&response),
                    last_round_usage,
                    RoundDecision::ToolBatch,
                )?;
                Some(
                    Box::pin(self.commit_hosted_round(
                        command.turn_id,
                        provider_operation,
                        outcome,
                        continuation_baton,
                        DurableOperationKind::ToolBatchContinuation {
                            turn_id: command.turn_id,
                            attempt_id,
                        },
                        continuation,
                        None,
                    ))
                    .await?,
                )
            } else {
                None
            };
            continuation_baton = continuation_operation;
            command.shared.begin_tools()?;
            let observation_budget_before_batch = self.observation_budget;
            let execution = Box::pin(self.execute_tool_batch(
                command,
                attempt_id,
                calls.clone(),
                result_entry_ids.clone(),
                tool_calls,
                provider_operation,
                &provider_plan,
                &response,
            ))
            .await;
            let execution = match execution {
                Ok(execution) => execution,
                Err(error) => {
                    command.cancellation.cancel();
                    // Partial items were never promoted. Reconstruct from
                    // durable effect evidence with the same allowance the
                    // original batch had before those items were clipped.
                    self.observation_budget = observation_budget_before_batch;
                    return Err(Box::pin(self.close_interrupted_tool_batch(
                        command,
                        attempt_id,
                        &calls,
                        &result_entry_ids,
                        provider_operation,
                        &response,
                        None,
                        error,
                    ))
                    .await);
                }
            };
            tool_calls = tool_calls.saturating_add(execution.dispatched);
            let mut batch = response.output().to_vec();
            batch.extend(execution.items);
            let mut completed_operations = execution.operations;
            completed_operations.push(provider_operation);
            let promoted = self
                .promote_tool_exchange_then_maybe_compact(
                    command,
                    attempt_id,
                    batch,
                    completed_operations,
                    provider_artifact_transition(&provider_plan, &response),
                )
                .await;
            self.finalize_continuation_on_request_budget(
                command.turn_id,
                continuation_baton,
                promoted,
            )
            .await?;
            let _ = self.apply_steering(command).await?;
            command.shared.begin_preparing()?;
        }
    }

    /// The restorable frame of a round paused at its customer tool batch.
    #[allow(
        clippy::too_many_arguments,
        reason = "the frame records every round input the restored batch needs"
    )]
    pub(super) fn tool_batch_continuation(
        &self,
        command: &PromptCommand,
        tool_calls: usize,
        attempt_id: AttemptId,
        provider_operation: OperationId,
        provider_plan: &ValidatedRequestPlan,
        response: &ModelResponse,
        calls: &[ToolCallBlock],
        result_entry_ids: &[EntryId],
    ) -> Result<RuntimeContinuation, AgentError> {
        let (model_admission_reference, policy_reference) = self.continuation_bindings();
        Ok(RuntimeContinuation::AwaitToolBatchV2(Box::new(
            AwaitToolBatchV2 {
                root: Self::continuation_root(command),
                config: DurableAgentConfig::from_config(self.config)?,
                budgets: self.turn_budgets,
                tool_calls: u64::try_from(tool_calls).map_err(|_| {
                    AgentError::new(AgentErrorKind::Internal, "tool call counter overflowed")
                })?,
                usage: command.shared.usage(),
                context_anchor: self.context_anchor,
                request_anchor: self
                    .request_anchor
                    .as_ref()
                    .map(plakous_model::ContextRequestAnchor::identity)
                    .transpose()
                    .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?,
                attempt_id,
                provider_operation,
                provider_request: provider_plan.request().clone(),
                provider_plan_digest: provider_plan.digest(),
                provider_response: response.clone(),
                working: WorkingProjection::default(),
                steering: self
                    .steering
                    .iter()
                    .map(|queued| ContinuationSteering {
                        content: queued.content.clone(),
                        operation_id: queued.operation_id,
                    })
                    .collect(),
                slots: calls
                    .iter()
                    .cloned()
                    .zip(result_entry_ids.iter().copied())
                    .map(
                        |(call, result_entry_id)| ContinuationToolSlot::NotAdmitted {
                            call,
                            result_entry_id,
                        },
                    )
                    .collect(),
                observation_budget: self.observation_budget,
                model_admission_reference,
                policy_reference,
            },
        )))
    }
}
