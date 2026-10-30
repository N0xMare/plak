use super::{
    Actor, AgentError, AgentErrorKind, AgentEventKind, Arc, AssertUnwindSafe, BTreeSet,
    ConversationItem, Driver, DriverRestoredStartup, DurableOperationState, EventContext,
    FutureExt, HistorySegmentKind, ModelError, ModelErrorPhase, ModelOperationRoot, OperationId,
    Ordering, PromptCommand, Provenance, QueuedSteering, RestoredContinuationFrame,
    RestoredEffectDecision, SessionStoreError, TerminalProposal, TurnResult, actor_panic_error,
    remove_known_turn,
};

impl Driver {
    pub(in crate::runtime::driver) async fn run_restored_startup(
        &mut self,
        restored: DriverRestoredStartup,
    ) -> Result<(), AgentError> {
        let DriverRestoredStartup {
            frame,
            effect,
            command,
        } = restored;
        self.active_root = Some(command.clone());
        command.shared.start()?;
        // This process's observation view has not seen the original start.
        // Seed its lifecycle before publishing the restored decision or tail.
        command.shared.events.publish(
            EventContext::turn(command.turn_id),
            AgentEventKind::TurnStarted,
        )?;
        self.active_model_root_scope = Some(
            Arc::clone(&self.model)
                .begin_operation_root(ModelOperationRoot::Turn(command.turn_id))
                .map_err(ModelError::from)
                .map_err(|error| self.project_model_error(error, ModelErrorPhase::Preparation))?,
        );
        command.shared.begin_model_streaming()?;
        command
            .shared
            .mark_telemetry_started(tokio::time::Instant::now());
        let pending_steering = self
            .authority
            .checkpoint()
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .filter(|record| !matches!(record.state(), DurableOperationState::Finalized(_)))
            .map(|record| OperationId(record.operation_id()))
            .collect::<BTreeSet<_>>();
        self.steering = frame
            .steering()
            .iter()
            .filter(|steering| pending_steering.contains(&steering.operation_id))
            .map(|steering| QueuedSteering {
                content: steering.content.clone(),
                operation_id: steering.operation_id,
            })
            .collect();
        self.admitted_round = None;
        self.noticed_job_terminals.clear();
        self.observed_job_terminals.clear();
        self.set_tool_preempt(if self.steering.is_empty() {
            plakous_tools::ToolPreempt::None
        } else {
            plakous_tools::ToolPreempt::Steer
        });
        self.turn_budgets = frame.budgets();
        self.context_anchor = frame.context_anchor();
        let descriptor = self.provider_configuration.effective().descriptor();
        self.request_anchor = frame.request_anchor().and_then(|identity| {
            self.authority
                .checkpoint()
                .manifest()
                .canonical()
                .context()
                .request_anchor()
                .filter(|anchor| {
                    anchor.matches_descriptor(descriptor)
                        && anchor.identity().ok() == Some(identity)
                })
                .cloned()
        });
        if frame.request_anchor().is_some() && self.request_anchor.is_none() {
            self.context_anchor = None;
        }
        let outcome = AssertUnwindSafe(async {
            match frame {
                RestoredContinuationFrame::EffectDecision(frame) => {
                    command.shared.begin_tools()?;
                    self.observation_budget = frame.observation_budget;
                    let RestoredEffectDecision {
                        invocation,
                        projection_receipt,
                        pending,
                    } = effect
                        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
                    Box::pin(self.resume_restored_turn(
                        &command,
                        *frame,
                        invocation,
                        projection_receipt,
                        pending,
                    ))
                    .await
                }
                RestoredContinuationFrame::ToolBatch(frame) => {
                    command.shared.begin_tools()?;
                    Box::pin(self.resume_hosted_tool_batch(&command, *frame)).await
                }
                RestoredContinuationFrame::ServerPause(frame) => {
                    Box::pin(self.resume_hosted_server_pause(&command, *frame)).await
                }
            }
        })
        .catch_unwind()
        .await;
        let (result, fatal_error) = if let Ok(result) = outcome {
            (result, None)
        } else {
            let error = actor_panic_error();
            (Err(error.clone()), Some(error))
        };
        let proposal = TerminalProposal::from_result(result);
        let recovery_required = proposal.recovery_required();
        self.commit_and_publish_terminal(&command, proposal).await?;
        self.active_root = None;
        self.active_model_root_scope = None;
        remove_known_turn(&self.known_turns, command.turn_id);
        if recovery_required {
            self.accepting.store(false, Ordering::Release);
        }
        if let Some(error) = fatal_error {
            return Err(error);
        }
        Ok(())
    }

    /// Runs one admitted root turn to its result.
    ///
    /// There is no wall clock on a turn. It ends when the model answers, when
    /// the round cap's wrap-up round answers, on cancellation, or on a bounded
    /// per-operation failure; a long build is not a reason to stop.
    pub(in crate::runtime::driver) async fn run_admitted_turn(
        &mut self,
        command: &PromptCommand,
    ) -> (Result<TurnResult, AgentError>, Option<AgentError>) {
        let outcome = AssertUnwindSafe(self.run_turn(command))
            .catch_unwind()
            .await;
        if let Ok(result) = outcome {
            (result, None)
        } else {
            let error = actor_panic_error();
            (Err(error.clone()), Some(error))
        }
    }

    pub(super) async fn run_turn(
        &mut self,
        command: &PromptCommand,
    ) -> Result<TurnResult, AgentError> {
        self.admitted_round = None;
        self.noticed_job_terminals.clear();
        self.observed_job_terminals.clear();
        self.run_in_memory_turn(command).await
    }

    pub(super) async fn run_in_memory_turn(
        &mut self,
        command: &PromptCommand,
    ) -> Result<TurnResult, AgentError> {
        self.run_turn_from_state(command, true, 0, None).await
    }

    /// Appends the turn's prompt to history after any pre-prompt compaction.
    pub(super) async fn append_turn_prompt(
        &mut self,
        command: &PromptCommand,
    ) -> Result<(), AgentError> {
        self.maybe_compact_before_turn_prompt(command).await?;
        // Refused before the prompt appends, so a refused turn adds no segment.
        if let Some(refusal) = self.segment_limit_refusal(command.turn_id) {
            return Err(refusal);
        }
        let prompt_entry = self.entry_id()?;
        let user_item = ConversationItem::new(
            prompt_entry,
            Actor::User,
            command.input.content_blocks().map_err(|_| {
                AgentError::new(AgentErrorKind::InvalidInput, "prompt input is invalid")
            })?,
            if command.instruction {
                Provenance::Instruction
            } else {
                Provenance::UserInput
            },
        )
        .map_err(|_| AgentError::new(AgentErrorKind::InvalidInput, "prompt item is invalid"))?;
        self.append_history_with_notices(
            command.turn_id,
            Some(HistorySegmentKind::RecentIntent),
            [user_item],
            true,
        )
        .await?;
        self.appended_prompt = Some(prompt_entry);
        Ok(())
    }
}
