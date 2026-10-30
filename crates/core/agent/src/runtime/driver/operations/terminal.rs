use super::{
    AgentError, AgentErrorKind, Arc, ArtifactTransition, CanonicalHistory, CheckpointId, CommandId,
    ContextStatusKind, ConversationView, Digest, Driver, DurableActorCommandEffect,
    DurableAgentError, DurableCommandDisposition, DurableCommandEnvelope, DurableCommandRecord,
    DurableOperationKind, DurableOperationRecord, DurableOperationState,
    DurableOperationTransition, DurableTerminalOutcome, DurableTerminalResultRecord, FailTurnShape,
    FamilyArtifact, HistorySegment, ModelErrorKind, ModelOperationRoot, OperationId,
    PendingTerminalResultRecord, ProviderOutcome, RoundDecision, RuntimeCheckpoint,
    RuntimeComponentKind, RuntimeContextState, RuntimeLifecycle, RuntimeOperationLedger,
    StoreAdmission, TERMINAL_RESULT_RECORD_VERSION, TerminalPromptTail, TurnConsequence, TurnId,
    TurnUsage, cancelled_error, encode_pending_terminal_record, encode_terminal_result_record,
    failure_preserves_artifact, failure_prompt_tail, finalized_operation_evidence,
    history_segment_kind, operation_turn_id, provider_outcome, runtime_component,
    runtime_manifest_error, trailing_prompt, working_lifecycle_for_root,
};

impl Driver {
    /// The consequence commit of a turn's end (spec 01 §3.5 items 1b and 2):
    /// the root's terminal record; the response's history segment, or the
    /// failure or cancellation note or the withheld-prompt replacement; the
    /// artifact transition; the closure of pending same-turn controls; and
    /// the finalization of every same-turn record, in one checkpoint CAS.
    pub(in crate::runtime::driver) async fn commit_turn_terminal(
        &mut self,
        command_id: &CommandId,
        turn_id: TurnId,
        operation_id: OperationId,
        consequence: TurnConsequence<'_>,
        usage: TurnUsage,
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError> {
        let closures = self.pending_turn_command_closures(turn_id, operation_id)?;
        let mut plan = self.commit_plan()?;
        plan.transitions = closures;
        plan.finalize_turn = true;
        let bytes = match consequence {
            TurnConsequence::Completed { response, artifact } => {
                plan.artifact = artifact;
                if !response.output().is_empty() {
                    plan.segments.push(
                        HistorySegment::new(
                            history_segment_kind(response.output()),
                            response.output().to_vec(),
                        )
                        .map_err(|_| {
                            AgentError::new(
                                AgentErrorKind::Internal,
                                "canonical history rejected an atomic semantic group",
                            )
                        })?,
                    );
                }
                crate::runtime::terminal::completed_terminal_evidence(
                    command_id,
                    turn_id,
                    operation_id,
                    response,
                    usage,
                )?
                .bytes()
                .to_vec()
            }
            TurnConsequence::Ended(error) => {
                // Pre-dispatch context admission never started a new provider
                // round. Clearing the family artifact here makes later
                // inspect/usage measure a smaller Current projection than the
                // denied prepare, so compact no-ops.
                plan.artifact = if failure_preserves_artifact(error) {
                    ArtifactTransition::Preserve
                } else {
                    ArtifactTransition::Clear
                };
                let (withhold_prompt, segment) = self
                    .terminal_prompt_tail(turn_id, error)
                    .await?
                    .into_commit()?;
                plan.withhold_prompt = withhold_prompt;
                plan.segments.extend(segment);
                let remedy = match self.recorded_fail_turn(turn_id, error)? {
                    Some(shape) => shape.remedy,
                    None => crate::runtime::agent::context_overflow_remedy(error, withhold_prompt)
                        .map(Into::into),
                };
                let error = DurableAgentError::from_error(error)?;
                let outcome = match error.kind {
                    AgentErrorKind::Cancelled => DurableTerminalOutcome::Cancelled(error),
                    AgentErrorKind::OutcomeUnknown => {
                        DurableTerminalOutcome::RecoveryRequired(error)
                    }
                    _ => DurableTerminalOutcome::Failed(error),
                };
                encode_pending_terminal_record(PendingTerminalResultRecord {
                    remedy,
                    version: TERMINAL_RESULT_RECORD_VERSION,
                    command_id: command_id.as_str().to_owned(),
                    turn_id,
                    operation_id,
                    outcome,
                    usage,
                })
                .map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "terminal turn failure could not be canonicalized",
                    )
                })?
            }
        };
        let root = self.root_terminal_state(operation_id, bytes)?;
        plan.transitions.push((operation_id, root));
        self.publish_commit_plan(turn_id, plan).await
    }

    /// What the terminal commit of a cancelled or failed root adds after its
    /// prompt. Canonical history only appends, so an unanswered prompt gets a
    /// harness note; a prompt that is itself the reason the turn could not be
    /// admitted to the model's window (the rest fits without it) is the one
    /// exception, because replaying it verbatim would refuse every later turn
    /// the same way. A `FailTurn` decision already recorded that measurement;
    /// only a failure no provider decision covers measures here.
    pub(super) async fn terminal_prompt_tail(
        &mut self,
        turn_id: TurnId,
        error: &AgentError,
    ) -> Result<TerminalPromptTail, AgentError> {
        if self.prompt_in_history != Some(turn_id) {
            return Ok(TerminalPromptTail::Unchanged);
        }
        let withhold = if matches!(
            error.kind(),
            AgentErrorKind::Cancelled | AgentErrorKind::OutcomeUnknown
        ) {
            false
        } else {
            match self.recorded_fail_turn(turn_id, error)? {
                Some(shape) => shape.prompt_withheld,
                None => self.failure_withholds_prompt(turn_id, error).await,
            }
        };
        let trailing = self.appended_prompt.and_then(|prompt| {
            trailing_prompt(
                self.authority.checkpoint().manifest().canonical().history(),
                Some(prompt),
            )
        });
        failure_prompt_tail(turn_id, error, withhold, trailing)
    }

    /// The terminal shape a recorded `FailTurn` decision committed this turn
    /// to for exactly `error`, if one is still owed its consequence.
    pub(super) fn recorded_fail_turn(
        &self,
        turn_id: TurnId,
        error: &AgentError,
    ) -> Result<Option<FailTurnShape>, AgentError> {
        let failure = DurableAgentError::from_error(error)?;
        let authority = self.authority.checkpoint();
        let mut newest = None;
        for record in authority.manifest().ledger().operations() {
            if operation_turn_id(record) != Some(turn_id)
                || matches!(record.state(), DurableOperationState::Finalized(_))
            {
                continue;
            }
            if let Some(ProviderOutcome {
                decision: RoundDecision::FailTurn(shape),
                ..
            }) = provider_outcome(record)?
            {
                newest = Some(shape);
            }
        }
        Ok(newest.filter(|shape| shape.failure == failure))
    }

    /// Whether a turn failing with `error` now would replace its prompt with
    /// the withheld-prompt note: the measurement a `FailTurn` decision
    /// records, made over the same state the terminal commit reads.
    pub(in crate::runtime::driver) async fn failure_withholds_prompt(
        &mut self,
        turn_id: TurnId,
        error: &AgentError,
    ) -> bool {
        self.prompt_in_history == Some(turn_id)
            && !matches!(
                error.kind(),
                AgentErrorKind::OutcomeUnknown | AgentErrorKind::Cancelled
            )
            && error.model_kind() == Some(ModelErrorKind::ContextLimit)
            && self.segment_refused_turn != Some(turn_id)
            && self.appended_prompt.is_some_and(|prompt| {
                trailing_prompt(
                    self.authority.checkpoint().manifest().canonical().history(),
                    Some(prompt),
                )
                .is_some()
            })
            && self.fits_without_trailing_prompt().await
    }

    /// Whether the projection without the trailing prompt fits the checked
    /// capacity, i.e. the prompt itself is what overflowed. A measurement
    /// that cannot be established keeps the prompt.
    pub(super) async fn fits_without_trailing_prompt(&mut self) -> bool {
        let input = {
            let authority = self.authority.checkpoint();
            let canonical = authority.manifest().canonical();
            let mut retained = canonical.history().segments().to_vec();
            retained.pop();
            let Ok(history) = CanonicalHistory::new(retained) else {
                return false;
            };
            ConversationView::from_shared(history, canonical.working().clone())
        };
        self.measure_context_input(input)
            .await
            .is_ok_and(|status| matches!(status.kind(), ContextStatusKind::Fits { .. }))
    }

    /// The byte length of this root's own prompt when it is still the last
    /// canonical segment, i.e. no round of the turn committed after it.
    /// The text bytes and provenance of this root's own prompt when it is
    /// still the last canonical segment.
    #[allow(
        clippy::too_many_lines,
        reason = "Queued cancellation publishes the command closure, prompt disposition, and terminal receipt in one checkpoint"
    )]
    #[allow(
        clippy::too_many_arguments,
        reason = "one atomic queued-cancel candidate closes the complete command and target tuple"
    )]
    pub(in crate::runtime::driver) async fn publish_atomic_queued_cancellation(
        &mut self,
        cancel_command_id: &CommandId,
        sequence: u64,
        turn_id: TurnId,
        binding: Digest,
        canonical_input: Vec<u8>,
        root_command_id: &CommandId,
        root_operation: OperationId,
        usage: TurnUsage,
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError> {
        if !matches!(
            self.operation_state(root_operation)?,
            DurableOperationState::Prepared
        ) {
            return Err(AgentError::new(
                AgentErrorKind::Internal,
                "queued cancellation root was already dispatched",
            ));
        }
        let checkpoint_id = self.checkpoint_id()?;
        let cancel_operation = self.take_operation_id()?;
        let durable_effect = DurableActorCommandEffect::CancelRequested;
        let envelope = DurableCommandEnvelope {
            version: 1,
            canonical_input,
            effect: durable_effect.clone(),
            rejection: None,
        };
        let bytes = serde_json::to_vec(&envelope).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "queued cancellation receipt could not be canonically encoded",
            )
        })?;
        let command_component = runtime_component(RuntimeComponentKind::Command, bytes.clone())?;
        let operation_request = runtime_component(RuntimeComponentKind::Operation, bytes)?;
        let error = DurableAgentError::from_error(&cancelled_error())?;
        let (history, ledger, context, family, lifecycle_root) = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            let history = manifest.canonical().history().clone();
            let lifecycle_root = manifest
                .lifecycle()
                .operation_root()
                .unwrap_or(ModelOperationRoot::Turn(turn_id));
            if manifest.ledger().commands().into_iter().all(|record| {
                record.command_id() != root_command_id.as_str()
                    || record.turn_id() != Some(turn_id)
                    || record.disposition() != DurableCommandDisposition::Accepted
            }) {
                return Err(AgentError::new(
                    AgentErrorKind::Internal,
                    "queued cancellation root command receipt is unavailable",
                ));
            }
            let record = manifest
                .ledger()
                .operation(root_operation.0)
                .ok_or_else(|| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "queued cancellation referenced an unknown root operation",
                    )
                })?;
            if !matches!(
                record.kind(),
                DurableOperationKind::Command {
                    turn_id: Some(root_turn)
                } if *root_turn == turn_id
            ) || !matches!(record.state(), DurableOperationState::Prepared)
            {
                return Err(AgentError::new(
                    AgentErrorKind::Internal,
                    "queued cancellation root was not prepared for its exact turn",
                ));
            }
            let terminal = DurableTerminalResultRecord {
                abandonment: None,
                remedy: None,
                version: TERMINAL_RESULT_RECORD_VERSION,
                command_id: root_command_id.as_str().to_owned(),
                turn_id,
                operation_id: root_operation,
                checkpoint_id,
                conversation_identity: history.identity(),
                outcome: DurableTerminalOutcome::Cancelled(error),
                usage,
            };
            let evidence = runtime_component(
                RuntimeComponentKind::Operation,
                encode_terminal_result_record(&terminal).map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "queued cancellation result could not be canonicalized",
                    )
                })?,
            )?;
            let command_evidence = runtime_component(
                RuntimeComponentKind::Operation,
                serde_json::to_vec(&durable_effect).map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "queued cancellation effect could not be canonicalized",
                    )
                })?,
            )?;
            let command_evidence =
                finalized_operation_evidence(&command_evidence, checkpoint_id, history.identity())?;
            let command = DurableCommandRecord::new(
                cancel_command_id.as_str(),
                sequence,
                Some(turn_id),
                binding,
                command_component,
                DurableCommandDisposition::Accepted,
                None,
            )
            .map_err(runtime_manifest_error)?;
            let cancel_operation = DurableOperationRecord::new(
                cancel_operation.0,
                DurableOperationKind::Command {
                    turn_id: Some(turn_id),
                },
                operation_request,
                vec![DurableOperationTransition::new(
                    checkpoint_id,
                    DurableOperationState::Finalized(command_evidence),
                )],
            )
            .map_err(runtime_manifest_error)?;
            let ledger = manifest
                .ledger()
                .append_command(command)
                .and_then(|ledger| {
                    ledger.transition_operation(
                        root_operation.0,
                        DurableOperationTransition::new(
                            checkpoint_id,
                            DurableOperationState::Finalized(evidence),
                        ),
                    )
                })
                .and_then(|ledger| ledger.append_operation(cancel_operation))
                .map_err(runtime_manifest_error)?;
            (
                history,
                ledger,
                manifest.canonical().context().retained_for_exact_resume(),
                manifest.family_artifact().cloned(),
                lifecycle_root,
            )
        };
        let lifecycle = working_lifecycle_for_root(lifecycle_root, &ledger)?;
        self.publish_checkpoint(
            StoreAdmission::Settlement,
            checkpoint_id,
            lifecycle,
            history,
            ledger,
            context,
            family,
        )
        .await
    }

    /// A `RecoveryRequired` terminal in one commit: the root's record and its
    /// finalization, leaving the unresolved operations outstanding.
    pub(in crate::runtime::driver) async fn persist_recovery_required(
        &mut self,
        command_id: &CommandId,
        turn_id: TurnId,
        operation_id: OperationId,
        error: &AgentError,
        usage: TurnUsage,
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError> {
        let bytes = encode_pending_terminal_record(PendingTerminalResultRecord {
            remedy: None,
            version: TERMINAL_RESULT_RECORD_VERSION,
            command_id: command_id.as_str().to_owned(),
            turn_id,
            operation_id,
            outcome: DurableTerminalOutcome::RecoveryRequired(DurableAgentError::from_error(
                error,
            )?),
            usage,
        })
        .map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "recovery-required terminal result could not be canonicalized",
            )
        })?;
        let root = self.root_terminal_state(operation_id, bytes)?;
        let closures = self.pending_turn_command_closures(turn_id, operation_id)?;
        let mut plan = self.commit_plan()?;
        for (control, state) in closures {
            plan.transitions.push((control, state));
            plan.finalize.insert(control);
        }
        plan.transitions.push((operation_id, root));
        plan.finalize.insert(operation_id);
        self.publish_commit_plan(turn_id, plan).await
    }

    /// The closures of this turn's controls that never applied: a turn's
    /// end settles them in its own commit.
    pub(super) fn pending_turn_command_closures(
        &self,
        turn_id: TurnId,
        root_operation: OperationId,
    ) -> Result<Vec<(OperationId, DurableOperationState)>, AgentError> {
        let authority = self.authority.checkpoint();
        authority
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .filter(|record| {
                record.operation_id() != root_operation.0
                    && operation_turn_id(record) == Some(turn_id)
                    && matches!(record.kind(), DurableOperationKind::Command { .. })
                    && matches!(
                        record.state(),
                        DurableOperationState::Prepared | DurableOperationState::Dispatched
                    )
            })
            .map(|record| {
                let evidence = runtime_component(
                    RuntimeComponentKind::Operation,
                    serde_json::to_vec(&(
                        "command-terminal-closure-v1",
                        record.operation_id(),
                        record.request().bytes(),
                    ))
                    .map_err(|_| {
                        AgentError::new(
                            AgentErrorKind::Internal,
                            "unapplied command evidence could not be encoded",
                        )
                    })?,
                )?;
                let state = if matches!(record.state(), DurableOperationState::Prepared) {
                    DurableOperationState::ProvenNotDispatched(evidence)
                } else {
                    DurableOperationState::Completed(evidence)
                };
                Ok((OperationId(record.operation_id()), state))
            })
            .collect()
    }

    /// The root's terminal state carrying `bytes`.
    pub(super) fn root_terminal_state(
        &self,
        operation_id: OperationId,
        bytes: Vec<u8>,
    ) -> Result<DurableOperationState, AgentError> {
        let terminal = runtime_component(RuntimeComponentKind::Operation, bytes)?;
        match self.operation_state(operation_id)? {
            DurableOperationState::Prepared => {
                Ok(DurableOperationState::ProvenNotDispatched(terminal))
            }
            DurableOperationState::Dispatched => Ok(DurableOperationState::Completed(terminal)),
            DurableOperationState::Completed(_)
            | DurableOperationState::ProvenNotDispatched(_)
            | DurableOperationState::DecisionRequired(_)
            | DurableOperationState::ContinuationRequired(_)
            | DurableOperationState::Claimed(_)
            | DurableOperationState::LocatorBound(_)
            | DurableOperationState::OutcomeUnknown(_)
            | DurableOperationState::Finalized(_) => Err(AgentError::new(
                AgentErrorKind::Internal,
                "root command cannot be terminalized from its current durable state",
            )),
        }
    }

    /// Publishes a checkpoint that may end a turn. A quiescent one whose
    /// ledger has grown past its high-water mark first drops the finalized
    /// evidence of earlier turns, so a session is not bounded by its history.
    /// Compaction publications never come here: their receipt is checked
    /// against an exact re-fold of the ledger they started from.
    #[allow(
        clippy::too_many_arguments,
        reason = "one atomic terminal successor and its consumed observations"
    )]
    pub(super) async fn publish_turn_terminal(
        &mut self,
        checkpoint_id: CheckpointId,
        lifecycle: RuntimeLifecycle,
        history: CanonicalHistory,
        ledger: RuntimeOperationLedger,
        context: RuntimeContextState,
        family: Option<FamilyArtifact>,
        notice_consumptions: &[u32],
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError> {
        let published = self
            .publish_checkpoint_consuming_jobs(
                StoreAdmission::Settlement,
                checkpoint_id,
                lifecycle,
                history,
                ledger,
                context,
                family,
                notice_consumptions,
            )
            .await?;
        Ok(published)
    }
}
