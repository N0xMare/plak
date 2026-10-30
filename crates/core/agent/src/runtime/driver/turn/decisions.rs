use super::{
    Actor, AgentError, AgentErrorKind, AttemptFailure, AttemptId, AttemptRetry, AwaitServerPauseV1,
    ContentBlock, ContinuationSteering, ConversationItem, DecidedRemedy, Driver,
    DurableAgentConfig, DurableAgentError, DurableOperationKind, FailTurnShape, HistorySegment,
    HistorySegmentKind, HostedRoundDisposition, ModelError, ModelErrorKind, ModelErrorPhase,
    ModelResponse, ModelRetryRisk, ModelRetryability, OperationId, PromptCommand, Provenance,
    ProviderAttemptResult, ProviderOutcome, RoundDecision, RuntimeContinuation, SessionStoreError,
    SettledRound, TextBlock, TurnConsequence, TurnId, Usage, ValidatedRequestPlan,
    WorkingProjection, cancelled_error, context_overflow_remedy, history_segment_kind,
    output_limit_retry_eligible, provider_artifact_transition, turn_lifecycle_error,
};

impl Driver {
    /// The decision on a round's provider attempt that ended in `error`,
    /// taken before anything acts on it (spec 01 §5.5): a retry or an
    /// overflow fold the budgets still allow, or the failure that ends the
    /// turn. The budgets are the only authority for each guard.
    pub(super) fn failed_attempt_decision(
        &self,
        error: &AgentError,
        over_budget: bool,
        continuation_baton: Option<OperationId>,
    ) -> AttemptFailure {
        if error.model_kind() == Some(ModelErrorKind::ContentFilteredIncomplete)
            && !error.model_context().is_some_and(|context| {
                context.retryability()
                    == ModelRetryability::Unsafe {
                        risk: ModelRetryRisk::ProviderHostedEffect,
                    }
            })
        {
            if self.turn_budgets.content_filter_retry_spent {
                return AttemptFailure::EndsTurn(self.model_failure(
                    ModelErrorKind::Policy,
                    "stream disconnected before completion: Incomplete response returned, reason: content_filter",
                    ModelErrorPhase::Terminal,
                ));
            }
            if self.model.content_filter_retry_instruction().is_some() {
                return AttemptFailure::Retry(RoundDecision::RetryContentFilter);
            }
        }
        if output_limit_retry_eligible(
            error.model_kind(),
            self.turn_budgets.output_limit_retry_taken,
            over_budget,
        ) {
            return AttemptFailure::Retry(RoundDecision::RetryOutputLimit);
        }
        if !self.turn_budgets.overflow_recovery_taken
            && !over_budget
            && continuation_baton.is_none()
            && error.model_error().is_some_and(|error| {
                let descriptor = self.provider_configuration.effective().descriptor();
                error.kind() == ModelErrorKind::ContextLimit
                    && error.context().is_none_or(|context| {
                        context.route() == descriptor.route()
                            && context.model() == descriptor.requested_model()
                    })
            })
        {
            return AttemptFailure::Retry(RoundDecision::OverflowFold);
        }
        AttemptFailure::EndsTurn(error.clone())
    }

    /// The `FailTurn` decision for `failure`, with the terminal shape the
    /// consequence commit must publish.
    pub(super) async fn fail_turn_decision(
        &mut self,
        turn_id: TurnId,
        failure: &AgentError,
    ) -> Result<RoundDecision, AgentError> {
        let failure = self.with_last_compaction_diagnostic(failure.clone());
        let prompt_withheld = self.failure_withholds_prompt(turn_id, &failure).await;
        Ok(RoundDecision::FailTurn(FailTurnShape {
            failure: DurableAgentError::from_error(&failure)?,
            prompt_withheld,
            remedy: context_overflow_remedy(&failure, prompt_withheld).map(DecidedRemedy::from),
        }))
    }

    /// The outcome record of a round's provider attempt under the budgets in
    /// force after `decision` and the steers pending when it was taken.
    pub(super) fn provider_outcome(
        &self,
        attempt: Result<&ModelResponse, &AgentError>,
        usage: Option<Usage>,
        decision: RoundDecision,
    ) -> Result<ProviderOutcome, AgentError> {
        Ok(ProviderOutcome {
            result: match attempt {
                Ok(response) => ProviderAttemptResult::Response(response.clone()),
                Err(error) => ProviderAttemptResult::Failure(DurableAgentError::from_error(error)?),
            },
            usage,
            decision,
            budgets: self.turn_budgets,
            steering: self
                .steering
                .iter()
                .map(|queued| queued.operation_id)
                .collect(),
        })
    }

    /// The decision commit of a round's provider attempt.
    pub(super) async fn record_provider_decision(
        &mut self,
        turn_id: TurnId,
        provider_operation: OperationId,
        attempt: Result<&ModelResponse, &AgentError>,
        usage: Option<Usage>,
        decision: RoundDecision,
        continuation_baton: Option<OperationId>,
    ) -> Result<(), AgentError> {
        let outcome = self.provider_outcome(attempt, usage, decision)?;
        Box::pin(self.finish_provider_operation(
            turn_id,
            provider_operation,
            attempt,
            &outcome,
            continuation_baton,
        ))
        .await
    }

    /// A retry decision in one commit (spec 01 §3.5 item 6): the failed
    /// attempt's outcome record, finalized with the note or guidance the
    /// decision committed the turn to.
    #[allow(
        clippy::too_many_arguments,
        reason = "the retry commit binds the exact attempt, its usage, decision, baton and note"
    )]
    pub(super) async fn commit_retry(
        &mut self,
        turn_id: TurnId,
        provider_operation: Option<OperationId>,
        attempt_error: Option<&AgentError>,
        usage: Option<Usage>,
        decision: RoundDecision,
        continuation_baton: Option<OperationId>,
        note: ConversationItem,
        kind: HistorySegmentKind,
    ) -> Result<(), AgentError> {
        let mut plan = self.commit_plan()?;
        if let (Some(provider_operation), Some(attempt_error)) = (provider_operation, attempt_error)
        {
            let outcome = self.provider_outcome(Err(attempt_error), usage, decision)?;
            self.stage_provider_outcome(
                &mut plan,
                provider_operation,
                Err(attempt_error),
                &outcome,
                continuation_baton,
            )?;
            plan.finalize.insert(provider_operation);
        }
        plan.segments
            .push(HistorySegment::new(kind, vec![note]).map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "canonical history rejected an atomic semantic group",
                )
            })?);
        Box::pin(self.publish_commit_plan(turn_id, plan)).await?;
        Ok(())
    }

    /// The content-filter guidance a `RetryContentFilter` decision commits
    /// the turn to.
    pub(super) fn content_filter_guidance(
        &self,
        command: &PromptCommand,
    ) -> Result<ConversationItem, AgentError> {
        let entry = super::operations::turn_note_entry_id(
            command.turn_id,
            b"plakous-agent/content-filter-guidance-v1",
        )
        .ok_or_else(|| AgentError::new(AgentErrorKind::Internal, "guidance identity is invalid"))?;
        let instruction = self
            .model
            .content_filter_retry_instruction()
            .ok_or_else(|| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "content-filter retry decided without guidance",
                )
            })?;
        ConversationItem::new(
            entry,
            Actor::User,
            vec![ContentBlock::Text(
                TextBlock::new(instruction, None).map_err(|_| {
                    AgentError::new(AgentErrorKind::Internal, "guidance text is invalid")
                })?,
            )],
            Provenance::Instruction,
        )
        .map_err(|_| AgentError::new(AgentErrorKind::Internal, "guidance item is invalid"))
    }

    /// Waits out any provider retry delay after committed guidance.
    pub(super) async fn await_content_filter_retry(
        &self,
        command: &PromptCommand,
        error: &AgentError,
    ) -> Result<(), AgentError> {
        if let Some(ModelRetryability::Eligible {
            retry_after: Some(delay),
        }) = error
            .model_context()
            .map(plakous_model::ModelErrorContext::retryability)
        {
            tokio::select! {
                () = command.cancellation.cancelled() => return Err(cancelled_error()),
                () = self.shutdown.cancelled() => return Err(cancelled_error()),
                () = tokio::time::sleep(std::time::Duration::from_millis(delay.as_millis())) => {}
            }
        }
        command.shared.continue_after_model()?;
        Ok(())
    }

    /// Settles an attempt that ended with a definite error: its decision commits
    /// first, then the round is retried or the turn fails.
    #[allow(
        clippy::too_many_arguments,
        reason = "the failed attempt's decision, retry commit and overflow fold stay in one ordered body"
    )]
    pub(super) async fn settle_failed_attempt(
        &mut self,
        command: &PromptCommand,
        error: AgentError,
        over_budget: bool,
        continuation_baton: Option<OperationId>,
        provider_operation: Option<OperationId>,
        attempt_error: Option<AgentError>,
        last_round_usage: Option<Usage>,
    ) -> Result<AttemptRetry, AgentError> {
        let decision = self.failed_attempt_decision(&error, over_budget, continuation_baton);
        let (decision, failure) = match decision {
            AttemptFailure::Retry(decision) => {
                match decision {
                    RoundDecision::RetryContentFilter => {
                        self.turn_budgets.content_filter_retry_spent = true;
                        // The filtered attempt does not count as a round.
                        self.turn_budgets.model_rounds =
                            self.turn_budgets.model_rounds.saturating_sub(1);
                    }
                    RoundDecision::RetryOutputLimit => {
                        self.turn_budgets.output_limit_retry_taken = true;
                    }
                    RoundDecision::OverflowFold => {
                        self.turn_budgets.overflow_recovery_taken = true;
                    }
                    _ => {}
                }
                (decision, None)
            }
            AttemptFailure::EndsTurn(failure) => (
                self.fail_turn_decision(command.turn_id, &failure).await?,
                Some(failure),
            ),
        };
        match decision {
            RoundDecision::RetryContentFilter => {
                let guidance = self.content_filter_guidance(command)?;
                let kind = history_segment_kind(std::slice::from_ref(&guidance));
                Box::pin(self.commit_retry(
                    command.turn_id,
                    provider_operation,
                    attempt_error.as_ref(),
                    last_round_usage,
                    decision,
                    continuation_baton,
                    guidance,
                    kind,
                ))
                .await?;
                self.await_content_filter_retry(command, &error).await?;
                return Ok(AttemptRetry::ContentFilter);
            }
            RoundDecision::RetryOutputLimit => {
                Box::pin(self.commit_output_limit_retry(
                    command,
                    provider_operation,
                    attempt_error.as_ref(),
                    last_round_usage,
                    continuation_baton,
                    &error,
                ))
                .await?;
                return Ok(AttemptRetry::OutputLimit);
            }
            _ => {}
        }
        if let (Some(provider_operation), Some(attempt_error)) =
            (provider_operation, attempt_error.as_ref())
        {
            self.record_provider_decision(
                command.turn_id,
                provider_operation,
                Err(attempt_error),
                last_round_usage,
                decision,
                continuation_baton,
            )
            .await?;
        }
        if let Some(failure) = failure {
            return Err(failure);
        }
        if Box::pin(self.compact_after_provider_overflow(command)).await? {
            // The refused operation entered ModelStreaming before
            // the provider classified it. A successful fold closes
            // that round and the resend starts from Preparing.
            if error
                .model_error()
                .is_some_and(ModelError::is_provider_context_overflow)
            {
                command.shared.continue_after_model()?;
            }
            // The refused round produced nothing and its operation
            // is finalized. The round is sent again as a new
            // operation on the smaller successor; the prompt is
            // already history and is not appended twice.
            self.turn_budgets.model_rounds = self.turn_budgets.model_rounds.saturating_sub(1);
            return Ok(AttemptRetry::OverflowFold);
        }
        Err(error)
    }

    /// Commits a round that requested no customer tools: a server pause or a
    /// continuing round resumes the loop; any other round ends the turn.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "each disposition's decision and history commit stays in one ordered body"
    )]
    pub(super) async fn settle_model_round(
        &mut self,
        command: &PromptCommand,
        disposition: &HostedRoundDisposition,
        tool_calls: usize,
        attempt_id: AttemptId,
        provider_operation: OperationId,
        provider_plan: &ValidatedRequestPlan,
        response: ModelResponse,
        last_round_usage: Option<Usage>,
        continuation_baton: Option<OperationId>,
    ) -> Result<SettledRound, AgentError> {
        if matches!(disposition, HostedRoundDisposition::ServerPaused) {
            let (model_admission_reference, policy_reference) = self.continuation_bindings();
            let continuation =
                RuntimeContinuation::AwaitServerPauseV1(Box::new(AwaitServerPauseV1 {
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
                    observation_budget: self.observation_budget,
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
                    model_admission_reference,
                    policy_reference,
                }));
            let outcome =
                self.provider_outcome(Ok(&response), last_round_usage, RoundDecision::Continue)?;
            // The decision, the restorable frame and the response's
            // history are one commit (spec 01 §3.5 item 5).
            let continuation_operation = Box::pin(self.commit_hosted_round(
                command.turn_id,
                provider_operation,
                outcome,
                continuation_baton,
                DurableOperationKind::ServerPauseContinuation {
                    turn_id: command.turn_id,
                    attempt_id,
                },
                continuation,
                Some((
                    &response,
                    provider_artifact_transition(provider_plan, &response),
                )),
            ))
            .await?;
            let _ = self.apply_steering(command).await?;
            command.shared.continue_after_model()?;
            return Ok(SettledRound::Continue(Some(continuation_operation)));
        }
        let continuing = disposition.continuing(!self.steering.is_empty());
        if continuing {
            // The decision and the response's history are one commit
            // (spec 01 §3.5 item 5).
            Box::pin(self.commit_continue_round(
                command.turn_id,
                provider_operation,
                &response,
                last_round_usage,
                continuation_baton,
                provider_artifact_transition(provider_plan, &response),
            ))
            .await?;
            let steering_pending = !self.steering.is_empty();
            if !self.apply_steering(command).await? && steering_pending {
                return Err(turn_lifecycle_error());
            }
            command.shared.continue_after_model()?;
            return Ok(SettledRound::Continue(None));
        }
        // The decision commit; the consequence commit follows once the
        // turn's process sessions are joined (spec 01 §3.5 item 1).
        self.record_provider_decision(
            command.turn_id,
            provider_operation,
            Ok(&response),
            last_round_usage,
            RoundDecision::Terminal,
            continuation_baton,
        )
        .await?;
        Box::pin(self.cleanup_process_sessions(command.turn_id)).await?;
        command.shared.begin_commit()?;
        let checkpoint = match Box::pin(self.commit_turn_terminal(
            &command.command_id,
            command.turn_id,
            command.operation_id,
            TurnConsequence::Completed {
                response: &response,
                artifact: provider_artifact_transition(provider_plan, &response),
            },
            command.shared.usage(),
        ))
        .await
        {
            Ok(checkpoint) => checkpoint,
            Err(error) => return Err(self.required_terminal_publication_failure(&error)),
        };
        Ok(SettledRound::Finished(Box::new(
            crate::runtime::terminal::completed_turn_result(
                command.turn_id,
                response,
                command.shared.usage(),
                checkpoint,
            ),
        )))
    }
}
