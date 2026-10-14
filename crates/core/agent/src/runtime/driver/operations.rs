//! Canonical checkpoint publication and operation transitions.

use crate::manifest::RuntimeContextState;
use crate::runtime::admission::validate_configuration_runtime_authority;
use crate::runtime::checkpoint::candidate::SuccessorFacts;
use crate::runtime::checkpoint::identity::history_segment_kind;
use crate::runtime::checkpoint::io::DurabilityCommitTelemetry;
use crate::runtime::continuation::{runtime_component, runtime_manifest_error};
use crate::runtime::control::{
    durable_command_effect, durable_control_error_kind, durable_rejection, effect_intent_bytes,
};
use crate::runtime::driver::Driver;
use crate::runtime::observation::{
    retained_tool_exchange_failure, shrink_tool_exchange_observations, tool_exchange_shrink_budget,
};
use crate::runtime::operation_record::{
    FailTurnShape, ProviderOutcome, RoundDecision, effect_operation_intent_digest,
    ledger_needs_retention, operation_kind_turn_id, operation_turn_id, provider_outcome,
    provider_terminal_state, retain_quiescent_ledger, working_lifecycle,
    working_lifecycle_for_root,
};
use crate::runtime::terminal::{
    DurableAgentError, DurableTerminalOutcome, DurableTerminalResultRecord,
    PendingTerminalResultRecord, TERMINAL_RESULT_RECORD_VERSION, cancelled_error,
    encode_pending_terminal_record, encode_terminal_result_record, finalized_operation_evidence,
};
use crate::runtime::{
    ArtifactTransition, DurableActorCommandEffect, DurableCommandEnvelope, OperationId,
    PromptCommand,
};
use crate::tool_runtime::{AuthorizedToolInvocation, ClaimedToolInvocation, EffectDispatchIntent};
use crate::{
    ActorCommandEffect, AgentError, AgentErrorKind, AgentEventKind, CheckpointId, CommandId,
    ControlError, DurableCommandDisposition, DurableCommandRecord, DurableOperationKind,
    DurableOperationRecord, DurableOperationState, DurableOperationTransition,
    EffectDecisionResolution, EventContext, FamilyArtifact, RuntimeCheckpoint,
    RuntimeComponentKind, RuntimeLifecycle, RuntimeOperationLedger, RuntimeWorkingRoot,
    SessionStoreError, StoreAdmission, TurnUsage,
};
use plakous_model::{
    ActiveFamilyArtifact, Actor, AttemptId, CanonicalHistory, ContentBlock, ContextStatusKind,
    ConversationItem, ConversationView, Digest, EntryId, HistorySegment, HistorySegmentKind,
    ModelCacheAffinity, ModelError, ModelErrorKind, ModelErrorPhase, ModelOperationRoot,
    ModelPreparationContext, ModelPreparationError, ModelRequest, ModelResponse,
    PreparedCallRecord, Provenance, RequestMetadata, TurnId, ValidationMode,
};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
impl Driver {
    pub(super) async fn record_job_terminals(&mut self) -> Result<bool, AgentError> {
        let prior = self.authority.checkpoint();
        let manifest = prior.manifest();
        if manifest.lifecycle() != &RuntimeLifecycle::Quiescent {
            return Ok(false);
        }
        let jobs = manifest
            .jobs()
            .with_terminals(&self.job_sessions.jobs())
            .map_err(runtime_manifest_error)?;
        if jobs == *manifest.jobs() {
            return Ok(false);
        }
        let history = manifest.canonical().history().clone();
        let ledger = manifest.ledger().clone();
        let context = manifest.canonical().context().retained_for_exact_resume();
        let family = manifest.family_artifact().cloned();
        let checkpoint_id = self.checkpoint_id()?;
        self.publish_checkpoint(
            StoreAdmission::Settlement,
            checkpoint_id,
            RuntimeLifecycle::Quiescent,
            history,
            ledger,
            context,
            family,
        )
        .await?;
        Ok(true)
    }

    /// Recovery publication can restore a full index before this driver runs.
    pub(super) async fn retain_settled_commands(&mut self) -> Result<(), AgentError> {
        let (history, ledger, context, family) = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            if manifest.lifecycle() != &RuntimeLifecycle::Quiescent {
                return Ok(());
            }
            (
                manifest.canonical().history().clone(),
                manifest.ledger().clone(),
                manifest.canonical().context().retained_for_exact_resume(),
                manifest.family_artifact().cloned(),
            )
        };
        let checkpoint_id = self.checkpoint_id()?;
        self.publish_checkpoint(
            StoreAdmission::Settlement,
            checkpoint_id,
            RuntimeLifecycle::Quiescent,
            history,
            ledger,
            context,
            family,
        )
        .await?;
        Ok(())
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "publication receives the closed successor checkpoint groups"
    )]
    pub(super) async fn publish_checkpoint(
        &mut self,
        admission: StoreAdmission,
        checkpoint_id: CheckpointId,
        lifecycle: RuntimeLifecycle,
        history: CanonicalHistory,
        ledger: RuntimeOperationLedger,
        context: RuntimeContextState,
        family_artifact: Option<FamilyArtifact>,
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError> {
        Box::pin(self.publish_checkpoint_consuming_jobs(
            admission,
            checkpoint_id,
            lifecycle,
            history,
            ledger,
            context,
            family_artifact,
            &[],
        ))
        .await
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one atomic successor and its consumed observations"
    )]
    async fn publish_checkpoint_consuming_jobs(
        &mut self,
        admission: StoreAdmission,
        checkpoint_id: CheckpointId,
        lifecycle: RuntimeLifecycle,
        history: CanonicalHistory,
        ledger: RuntimeOperationLedger,
        context: RuntimeContextState,
        family_artifact: Option<FamilyArtifact>,
        notice_consumptions: &[u32],
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError> {
        // Inside a running Code Cell every store failure is recovery
        // authoritative, so its commits are settlement; each nested call
        // meets the ordinary threshold before it journals anything instead
        // (`refuse_nested_under_store_pressure`).
        let admission =
            if admission == StoreAdmission::Ordinary && self.local_composition.is_in_flight() {
                StoreAdmission::Settlement
            } else {
                admission
            };
        let context = context
            .with_request_anchor(self.request_anchor.clone())
            .map_err(runtime_manifest_error)?;
        let retain = lifecycle == RuntimeLifecycle::Quiescent && ledger_needs_retention(&ledger);
        let ledger = if retain {
            // Terminal authority can precede delivery to a still-owned root.
            let owned_turns = self
                .active_root
                .iter()
                .chain(self.roots.iter())
                .map(|root| root.turn_id)
                .collect();
            retain_quiescent_ledger(&ledger, &owned_turns)?
        } else {
            ledger
        };
        let observation = self
            .local_composition
            .as_ref()
            .and_then(|state| state.interpreter_observation().cloned());
        let telemetry = DurabilityCommitTelemetry {
            emitter: Arc::clone(&self.telemetry),
            clock: Arc::clone(&self.clock),
            context: self.telemetry_context_for_turn(
                self.active_root.as_ref().map(|command| command.turn_id),
            ),
        };
        // Keep the owner's large publication future out of the nested driver
        // frame; embedding both frames exceeds the existing test stack budget.
        let carries_job_results = history
            != *self.authority.checkpoint().manifest().canonical().history()
            && history
                .segments()
                .into_iter()
                .last()
                .is_some_and(|segment| segment.kind() == HistorySegmentKind::ToolExchange);
        let mut consumed = notice_consumptions.to_vec();
        if carries_job_results {
            consumed.extend_from_slice(&self.pending_job_consumptions);
        }
        let jobs = self
            .authority
            .checkpoint()
            .manifest()
            .jobs()
            .updated(
                plakous_tools::internal::next_job_number(&self.job_sessions),
                if carries_job_results {
                    self.pending_job_promotions.clone()
                } else {
                    Vec::new()
                },
                &consumed,
            )
            .and_then(|jobs| jobs.with_terminals(&self.job_sessions.jobs()))
            .map_err(runtime_manifest_error)?;
        let prior = self.authority.checkpoint();
        let newly_finished = jobs
            .rows()
            .iter()
            .filter(|row| {
                row.terminal.is_some()
                    && prior
                        .manifest()
                        .jobs()
                        .rows()
                        .iter()
                        .find(|before| before.number == row.number)
                        .is_none_or(|before| before.terminal.is_none())
            })
            .map(|row| row.number)
            .collect::<Vec<_>>();
        let published = Box::pin(self.authority.publish(
            SuccessorFacts {
                checkpoint_id,
                lifecycle,
                history,
                ledger,
                jobs,
                context,
                family: family_artifact,
            },
            observation,
            &telemetry,
            admission,
        ))
        .await?;
        if !consumed.is_empty()
            && let Some(host) = &self.job_host
        {
            let terminal = self
                .job_sessions
                .jobs()
                .into_iter()
                .filter(|job| {
                    consumed.contains(&job.number)
                        && job.status != plakous_tools::JobStatus::Running
                })
                .collect::<Vec<_>>();
            host.observe_terminal(self.job_owner, &terminal);
        }
        plakous_tools::internal::consume_jobs(&self.job_sessions, &consumed);
        for number in newly_finished {
            let context = self
                .active_root
                .as_ref()
                .map_or_else(EventContext::agent, |root| EventContext::turn(root.turn_id));
            let _ = self
                .events
                .publish(context, AgentEventKind::JobFinished { number });
        }
        if carries_job_results {
            if let Some(root) = &self.active_root {
                for row in &self.pending_job_promotions {
                    let _ = root.shared.events.publish(
                        EventContext::turn(root.turn_id),
                        AgentEventKind::JobPromoted { number: row.number },
                    );
                }
            }
            self.pending_job_promotions.clear();
            self.pending_job_consumptions.clear();
        }
        if retain {
            let kept = published
                .manifest()
                .ledger()
                .commands()
                .into_iter()
                .map(|command| command.command_id().to_owned())
                .collect::<BTreeSet<_>>();
            self.command_ledger
                .retain(|command_id, _| kept.contains(command_id.as_str()));
        }
        Ok(published)
    }

    /// Prepares one operation. The caller classifies the commit: preparing
    /// new work is ordinary, recording a started or settled fact is settlement.
    pub(super) async fn prepare_operation(
        &mut self,
        admission: StoreAdmission,
        turn_id: TurnId,
        kind: DurableOperationKind,
        binding: Digest,
        recoverable_input: Option<Vec<u8>>,
        segment: Option<HistorySegment>,
    ) -> Result<OperationId, AgentError> {
        let operation_id = self.take_operation_id()?;
        self.prepare_operation_with_id(
            admission,
            turn_id,
            operation_id,
            kind,
            binding,
            recoverable_input,
            segment,
        )
        .await
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the admission class travels with the exact prepared record"
    )]
    pub(super) async fn prepare_operation_with_id(
        &mut self,
        admission: StoreAdmission,
        turn_id: TurnId,
        operation_id: OperationId,
        kind: DurableOperationKind,
        binding: Digest,
        recoverable_input: Option<Vec<u8>>,
        segment: Option<HistorySegment>,
    ) -> Result<OperationId, AgentError> {
        let checkpoint_id = self.checkpoint_id()?;
        let (mut history, mut ledger, context, family) = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            (
                manifest.canonical().history().clone(),
                manifest.ledger().clone(),
                manifest.canonical().context().retained_for_exact_resume(),
                manifest.family_artifact().cloned(),
            )
        };
        if let Some(segment) = segment {
            history = history.append(segment).map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "canonical history rejected an atomic semantic group",
                )
            })?;
        }
        let request = runtime_component(
            RuntimeComponentKind::Operation,
            recoverable_input.unwrap_or_else(|| binding.bytes().to_vec()),
        )?;
        let record = DurableOperationRecord::new(
            operation_id.0,
            kind,
            request,
            vec![DurableOperationTransition::new(
                checkpoint_id,
                DurableOperationState::Prepared,
            )],
        )
        .map_err(runtime_manifest_error)?;
        ledger = ledger
            .append_operation(record)
            .map_err(runtime_manifest_error)?;
        let lifecycle = working_lifecycle(turn_id, &ledger)?;
        self.publish_checkpoint(
            admission,
            checkpoint_id,
            lifecycle,
            history,
            ledger,
            context,
            family,
        )
        .await?;
        Ok(operation_id)
    }

    pub(super) async fn claim_mutating_effect(
        &mut self,
        turn_id: TurnId,
        effect_operation: OperationId,
        intent: EffectDispatchIntent,
        authorized: AuthorizedToolInvocation,
    ) -> Result<ClaimedToolInvocation, AgentError> {
        self.certify_mutating_effect_recovery()?;
        let claim_evidence_digest = self
            .persist_effect_claim(turn_id, effect_operation, &intent)
            .await?;
        let tools = self.tools.clone();
        tools
            .claim_effect(authorized, intent, claim_evidence_digest)
            .map_err(|error| {
                self.accepting.store(false, Ordering::Release);
                AgentError::from_tool(&error)
            })
    }

    async fn persist_effect_claim(
        &mut self,
        turn_id: TurnId,
        effect_operation: OperationId,
        intent: &EffectDispatchIntent,
    ) -> Result<Digest, AgentError> {
        if intent.digest()
            != effect_operation_intent_digest(&self.authority.checkpoint(), effect_operation)?
        {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "effect claim disagrees with the prepared durable intent",
            ));
        }
        let kind = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            let effect = manifest
                .ledger()
                .operation(effect_operation.0)
                .ok_or_else(|| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "effect claim referenced an unknown prepared operation",
                    )
                })?;
            if !matches!(effect.kind(), DurableOperationKind::Effect { .. })
                || !matches!(effect.state(), DurableOperationState::Prepared)
            {
                return Err(AgentError::new(
                    AgentErrorKind::InvalidInput,
                    "effect claim is not bound to a prepared effect",
                ));
            }
            effect.kind().clone()
        };
        if operation_kind_turn_id(&kind) != Some(turn_id) {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "effect claim targets another turn",
            ));
        }
        let evidence = runtime_component(
            RuntimeComponentKind::Operation,
            effect_intent_bytes(intent)?,
        )?;
        let claim_evidence_digest = evidence.integrity();
        self.transition_operation(
            turn_id,
            effect_operation,
            DurableOperationState::Claimed(evidence),
        )
        .await?;
        Ok(claim_evidence_digest)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the durable command receipt carries the complete closed classification tuple"
    )]
    pub(super) async fn persist_command_classification(
        &mut self,
        command_id: &CommandId,
        sequence: u64,
        turn_id: Option<TurnId>,
        binding: Digest,
        canonical_input: Vec<u8>,
        effect: ActorCommandEffect,
        rejection: Option<&ControlError>,
    ) -> Result<Option<OperationId>, AgentError> {
        let checkpoint_id = self.checkpoint_id()?;
        let durable_effect = durable_command_effect(effect)?;
        let envelope = DurableCommandEnvelope {
            version: 1,
            canonical_input,
            effect: durable_effect,
            rejection: rejection.map(durable_rejection),
        };
        let bytes = serde_json::to_vec(&envelope).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "command receipt could not be canonically encoded",
            )
        })?;
        let component = runtime_component(RuntimeComponentKind::Command, bytes.clone())?;
        let operation = runtime_component(RuntimeComponentKind::Operation, bytes)?;
        let accepted = rejection.is_none();
        // Root and steering receipts begin work; cancel, decision and shutdown
        // receipts settle work already admitted and must fit in the reserve.
        let admission = match effect {
            ActorCommandEffect::Root
            | ActorCommandEffect::Steer(_)
            | ActorCommandEffect::Rejected => StoreAdmission::Ordinary,
            ActorCommandEffect::Cancel(_)
            | ActorCommandEffect::EffectDecision(_)
            | ActorCommandEffect::Shutdown => StoreAdmission::Settlement,
        };
        let operation_id = accepted.then(|| self.take_operation_id()).transpose()?;
        let (history, ledger, context, family, prior_lifecycle) = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            let operation_turn_id = if matches!(effect, ActorCommandEffect::Root) {
                turn_id
            } else {
                match manifest.lifecycle() {
                    RuntimeLifecycle::Working {
                        root: RuntimeWorkingRoot::Turn { turn_id, .. },
                        ..
                    } => Some(*turn_id),
                    RuntimeLifecycle::Working {
                        root: RuntimeWorkingRoot::SessionWorkflow(_),
                        ..
                    } => {
                        return Err(AgentError::new(
                            AgentErrorKind::Internal,
                            "turn command cannot join an active session workflow",
                        ));
                    }
                    RuntimeLifecycle::Quiescent => turn_id,
                }
            };
            let record = DurableCommandRecord::new(
                command_id.as_str(),
                sequence,
                turn_id,
                binding,
                component,
                if accepted {
                    DurableCommandDisposition::Accepted
                } else {
                    DurableCommandDisposition::Rejected
                },
                rejection.map(|error| durable_control_error_kind(error.kind()).to_owned()),
            )
            .map_err(runtime_manifest_error)?;
            let mut ledger = manifest
                .ledger()
                .append_command(record)
                .map_err(runtime_manifest_error)?;
            if let Some(operation_id) = operation_id {
                ledger = ledger
                    .append_operation(
                        DurableOperationRecord::new(
                            operation_id.0,
                            DurableOperationKind::Command {
                                turn_id: operation_turn_id,
                            },
                            operation,
                            vec![DurableOperationTransition::new(
                                checkpoint_id,
                                DurableOperationState::Prepared,
                            )],
                        )
                        .map_err(runtime_manifest_error)?,
                    )
                    .map_err(runtime_manifest_error)?;
            }
            (
                manifest.canonical().history().clone(),
                ledger,
                manifest.canonical().context().retained_for_exact_resume(),
                manifest.family_artifact().cloned(),
                manifest.lifecycle().clone(),
            )
        };
        let lifecycle = if accepted {
            let lifecycle_turn = match prior_lifecycle {
                RuntimeLifecycle::Working {
                    root: RuntimeWorkingRoot::Turn { turn_id, .. },
                    ..
                } => turn_id,
                RuntimeLifecycle::Working {
                    root: RuntimeWorkingRoot::SessionWorkflow(_),
                    ..
                } => {
                    return Err(AgentError::new(
                        AgentErrorKind::Internal,
                        "turn command cannot join an active session workflow",
                    ));
                }
                RuntimeLifecycle::Quiescent => turn_id.unwrap_or(
                    TurnId::try_from(self.run_id.as_uuid())
                        .map_err(|error| AgentError::from_identity(&error))?,
                ),
            };
            working_lifecycle(lifecycle_turn, &ledger)?
        } else {
            prior_lifecycle
        };
        self.publish_checkpoint(
            admission,
            checkpoint_id,
            lifecycle,
            history,
            ledger,
            context,
            family,
        )
        .await?;
        Ok(operation_id)
    }

    pub(super) async fn complete_command_operation(
        &mut self,
        turn_id: TurnId,
        operation_id: OperationId,
        effect: ActorCommandEffect,
    ) -> Result<(), AgentError> {
        let mut plan = self.commit_plan()?;
        if matches!(
            self.operation_state(operation_id)?,
            DurableOperationState::Prepared
        ) {
            plan.transitions
                .push((operation_id, DurableOperationState::Dispatched));
        }
        let evidence = runtime_component(
            RuntimeComponentKind::Operation,
            serde_json::to_vec(&durable_command_effect(effect)?).map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "command effect evidence could not be encoded",
                )
            })?,
        )?;
        plan.transitions
            .push((operation_id, DurableOperationState::Completed(evidence)));
        plan.finalize.insert(operation_id);
        self.publish_commit_plan(turn_id, plan).await?;
        Ok(())
    }

    /// The consequence commit of a turn's end (spec 01 §3.5 items 1b and 2):
    /// the root's terminal record; the response's history segment, or the
    /// failure or cancellation note or the withheld-prompt replacement; the
    /// artifact transition; the closure of pending same-turn controls; and
    /// the finalization of every same-turn record, in one checkpoint CAS.
    pub(super) async fn commit_turn_terminal(
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
    async fn terminal_prompt_tail(
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
    fn recorded_fail_turn(
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
    pub(super) async fn failure_withholds_prompt(
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
    async fn fits_without_trailing_prompt(&mut self) -> bool {
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
        clippy::too_many_arguments,
        reason = "one atomic queued-cancel candidate closes the complete command and target tuple"
    )]
    pub(super) async fn publish_atomic_queued_cancellation(
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
    pub(super) async fn persist_recovery_required(
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
    fn pending_turn_command_closures(
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
    fn root_terminal_state(
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
    pub(super) async fn promote_tool_exchange_within_next_round_capacity(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        mut batch: Vec<ConversationItem>,
        operations: Vec<OperationId>,
        artifact_transition: ArtifactTransition,
    ) -> Result<Option<AgentError>, AgentError> {
        match self
            .admit_pending_tool_exchange(command, &batch, &artifact_transition)
            .await
        {
            Ok(()) => {
                self.promote_completed_operations_with_history(
                    command.turn_id,
                    Some(HistorySegmentKind::ToolExchange),
                    batch,
                    operations,
                    artifact_transition,
                )
                .await?;
                Ok(None)
            }
            Err(overflow)
                if overflow
                    .model_error()
                    .and_then(ModelError::context_capacity)
                    .is_some() =>
            {
                // Never fail the turn with the unshrunk overflow after the
                // retained projection has been truncated. Inspect and compact
                // see the promoted checkpoint; the operator error must match.
                // A fair split of the excess is tried first; if the estimate
                // misses, every result is exhausted as before.
                let budget = overflow
                    .model_error()
                    .and_then(ModelError::context_capacity)
                    .and_then(|failure| tool_exchange_shrink_budget(&batch, failure.overflow()));
                let (mut shrunk, mut replacements) =
                    shrink_tool_exchange_observations(batch.clone(), budget)?;
                let mut shrunk_admit = self
                    .admit_pending_tool_exchange(command, &shrunk, &artifact_transition)
                    .await;
                if budget.is_some()
                    && shrunk_admit.as_ref().is_err_and(|error| {
                        error
                            .model_error()
                            .and_then(ModelError::context_capacity)
                            .is_some()
                    })
                {
                    (shrunk, replacements) = shrink_tool_exchange_observations(batch, None)?;
                    shrunk_admit = self
                        .admit_pending_tool_exchange(command, &shrunk, &artifact_transition)
                        .await;
                }
                batch = shrunk;
                self.observation_budget.mark_capacity_exhausted();
                self.promote_completed_operations_with_history(
                    command.turn_id,
                    Some(HistorySegmentKind::ToolExchange),
                    batch,
                    operations,
                    artifact_transition,
                )
                .await?;
                for replacement in replacements {
                    let _ = command.shared.events.publish(
                        EventContext::call(command.turn_id, attempt_id, replacement.call_id),
                        AgentEventKind::ObservationReplaced {
                            bytes_before: replacement.bytes_before,
                            bytes_after: replacement.bytes_after,
                        },
                    );
                }
                retained_tool_exchange_failure(shrunk_admit)
            }
            Err(error) => Err(error),
        }
    }

    async fn admit_pending_tool_exchange(
        &mut self,
        command: &PromptCommand,
        batch: &[ConversationItem],
        artifact_transition: &ArtifactTransition,
    ) -> Result<(), AgentError> {
        if batch.is_empty() {
            return Ok(());
        }
        let segment = HistorySegment::new(HistorySegmentKind::ToolExchange, batch.to_vec())
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "pending tool exchange could not be grouped for admission",
                )
            })?;
        let current = self.conversation();
        let history = current.history().clone().append(segment).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "pending tool exchange could not be appended for admission",
            )
        })?;
        let view = ConversationView::from_shared(history, current.working().clone());
        let attempt_id = self.attempt_id()?;
        let prepared_at = self.clock.now();
        let request = ModelRequest::from_parts(
            self.stable_request_prefix.clone(),
            view,
            RequestMetadata::for_turn(self.run_id, command.turn_id, attempt_id, prepared_at),
        )
        .map_err(|error| {
            self.project_model_error(
                ModelError::new(
                    ModelErrorKind::InvalidRequest,
                    format!("canonical model request is invalid: {error}"),
                ),
                ModelErrorPhase::Preparation,
            )
        })?;
        let descriptor = self.provider_configuration.effective().descriptor().clone();
        let plan = descriptor
            .validate_request(request, ValidationMode::Strict)
            .map_err(ModelError::from)
            .map_err(|error| self.project_model_error(error, ModelErrorPhase::Preparation))?;
        descriptor
            .preflight_context_admission_requirement(plan.request().context_admission_requirement())
            .map_err(ModelPreparationError::from)
            .map_err(ModelError::from)
            .map_err(|error| self.project_model_error(error, ModelErrorPhase::Preparation))?;
        let authority = Arc::clone(&self.model)
            .revalidate_runtime_authority(prepared_at)
            .await
            .map_err(ModelPreparationError::from)
            .map_err(ModelError::from)
            .map_err(|error| self.project_model_error(error, ModelErrorPhase::Preparation))?;
        validate_configuration_runtime_authority(
            &self.provider_configuration,
            &descriptor,
            &authority,
            prepared_at,
        )
        .map_err(ModelError::from)
        .map_err(|error| self.project_model_error(error, ModelErrorPhase::Preparation))?;
        let (active_family_artifact, cache_affinity) =
            self.pending_model_preparation_state(artifact_transition)?;
        let mut preparation_context = ModelPreparationContext::new(authority, prepared_at)
            .with_cache_affinity(cache_affinity);
        preparation_context = self.with_previous_response_metadata(&plan, preparation_context)?;
        if let Some(artifact) = active_family_artifact {
            preparation_context = preparation_context.with_active_family_artifact(artifact);
        }
        Arc::clone(&self.model)
            .prepare_call(plan, preparation_context)
            .map_err(ModelError::from)
            .map_err(|error| self.project_model_error(error, ModelErrorPhase::Preparation))?;
        Ok(())
    }

    /// Artifact the next model round will see after this tool-exchange promote.
    ///
    /// Peek must not use the still-current checkpoint artifact: that envelope
    /// covers the previous round and treats this round's encrypted output as an
    /// unauthorized replay tail.
    fn pending_model_preparation_state(
        &self,
        transition: &ArtifactTransition,
    ) -> Result<(Option<ActiveFamilyArtifact>, ModelCacheAffinity), AgentError> {
        let (current, cache_affinity) = self.active_model_preparation_state()?;
        let artifact = match transition {
            ArtifactTransition::Preserve => current,
            ArtifactTransition::Clear => None,
            ArtifactTransition::Replace { candidate, .. } => Some(
                FamilyArtifact::from_candidate(candidate)
                    .and_then(|artifact| artifact.to_active())
                    .map_err(runtime_manifest_error)?,
            ),
        };
        Ok((artifact, cache_affinity))
    }

    pub(super) async fn promote_completed_operations_with_history<I, O>(
        &mut self,
        turn_id: TurnId,
        kind: Option<HistorySegmentKind>,
        items: I,
        required: O,
        artifact_transition: ArtifactTransition,
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError>
    where
        I: IntoIterator<Item = ConversationItem>,
        O: IntoIterator<Item = OperationId>,
    {
        let items = items.into_iter().collect::<Vec<_>>();
        let segments = if items.is_empty() {
            Vec::new()
        } else {
            vec![
                HistorySegment::new(kind.unwrap_or_else(|| history_segment_kind(&items)), items)
                    .map_err(|_| {
                        AgentError::new(
                            AgentErrorKind::Internal,
                            "canonical history rejected an atomic semantic group",
                        )
                    })?,
            ]
        };
        let mut plan = self.commit_plan()?;
        plan.segments = segments;
        plan.finalize.extend(required);
        plan.artifact = artifact_transition;
        self.publish_commit_plan(turn_id, plan).await
    }

    /// A plan for the next atomic successor of the current authority.
    pub(super) fn commit_plan(&mut self) -> Result<CommitPlan, AgentError> {
        Ok(CommitPlan {
            checkpoint_id: self.checkpoint_id()?,
            transitions: Vec::new(),
            appended: Vec::new(),
            finalize: BTreeSet::new(),
            finalize_turn: false,
            withhold_prompt: false,
            segments: Vec::new(),
            artifact: ArtifactTransition::Preserve,
            notices: Vec::new(),
        })
    }

    /// Publishes `plan` as one checkpoint CAS: its ledger edits, its history,
    /// the artifact transition and the finalization of every operation it
    /// names, bound to the committed history (spec 01 §3.5).
    #[allow(
        clippy::too_many_lines,
        reason = "one atomic successor is checked and assembled in one place"
    )]
    pub(super) async fn publish_commit_plan(
        &mut self,
        turn_id: TurnId,
        plan: CommitPlan,
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError> {
        let CommitPlan {
            checkpoint_id,
            transitions,
            appended,
            finalize,
            finalize_turn,
            withhold_prompt,
            segments,
            artifact: artifact_transition,
            notices,
        } = plan;
        let mut required = finalize;
        let edits_ledger = !transitions.is_empty() || !appended.is_empty();
        let (history, mut ledger, context, prior_family) = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            let rejected = || {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "canonical history rejected an atomic semantic group",
                )
            };
            let mut history = if withhold_prompt {
                let mut retained = manifest.canonical().history().segments().to_vec();
                retained.pop();
                CanonicalHistory::new(retained).map_err(|_| rejected())?
            } else {
                manifest.canonical().history().clone()
            };
            for segment in segments {
                history = history.append(segment).map_err(|_| rejected())?;
            }
            let mut ledger = manifest.ledger().clone();
            for (operation_id, state) in transitions {
                ledger = ledger
                    .transition_operation(
                        operation_id.0,
                        DurableOperationTransition::new(checkpoint_id, state),
                    )
                    .map_err(runtime_manifest_error)?;
            }
            for record in appended {
                ledger = ledger
                    .append_operation(record)
                    .map_err(runtime_manifest_error)?;
            }
            (
                history,
                ledger,
                manifest.canonical().context().retained_for_exact_resume(),
                manifest.family_artifact().cloned(),
            )
        };
        if finalize_turn {
            required.extend(settled_turn_operations(&ledger, turn_id)?);
        }
        if (required.is_empty() && !edits_ledger)
            || required.iter().any(|operation_id| {
                !ledger.operations().into_iter().any(|record| {
                    record.operation_id() == operation_id.0
                        && operation_turn_id(record) == Some(turn_id)
                        && (matches!(
                            record.state(),
                            DurableOperationState::Completed(_)
                                | DurableOperationState::ProvenNotDispatched(_)
                        ) || (matches!(
                            record.kind(),
                            DurableOperationKind::ToolBatchContinuation { .. }
                                | DurableOperationKind::ServerPauseContinuation { .. }
                        ) && matches!(
                            record.state(),
                            DurableOperationState::ContinuationRequired(_)
                        )))
                })
            })
        {
            return Err(AgentError::new(
                AgentErrorKind::Internal,
                "canonical promotion lacks a proven completed operation",
            ));
        }
        let family = match artifact_transition {
            ArtifactTransition::Preserve => prior_family,
            ArtifactTransition::Clear => None,
            ArtifactTransition::Replace {
                candidate,
                plan,
                response_output_items,
            } => {
                let covered_items = plan
                    .request()
                    .input()
                    .items()
                    .len()
                    .checked_add(response_output_items)
                    .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
                let covered = ConversationView::from_shared(
                    history.clone(),
                    plakous_model::WorkingProjection::default(),
                )
                .canonical_item_prefix_identity(covered_items)
                .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                candidate
                    .validate_for_commit(&plan, covered, response_output_items)
                    .map_err(|_| {
                        AgentError::new(
                            AgentErrorKind::Model,
                            "family artifact does not match the committed provider response",
                        )
                    })?;
                let prepared = ledger
                    .operations()
                    .into_iter()
                    .filter(|record| {
                        required.contains(&OperationId(record.operation_id()))
                            && matches!(record.kind(), DurableOperationKind::Provider { .. })
                    })
                    .map(|record| {
                        serde_json::from_slice::<PreparedCallRecord>(record.request().bytes())
                            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .find(|prepared| prepared.projection().plan() == plan.digest())
                    .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
                Some(crate::runtime::family_artifact::promote_family_artifact(
                    &candidate,
                    &prepared,
                    &history,
                    &context,
                    plan.request().input().items().len(),
                    response_output_items,
                )?)
            }
        };
        for operation_id in required {
            let evidence = ledger
                .operation(operation_id.0)
                .and_then(|record| match record.state() {
                    DurableOperationState::Completed(evidence)
                    | DurableOperationState::ProvenNotDispatched(evidence)
                    | DurableOperationState::ContinuationRequired(evidence)
                        if matches!(
                            record.kind(),
                            DurableOperationKind::ToolBatchContinuation { .. }
                                | DurableOperationKind::ServerPauseContinuation { .. }
                        ) || !matches!(
                            record.state(),
                            DurableOperationState::ContinuationRequired(_)
                        ) =>
                    {
                        Some(evidence)
                    }
                    _ => None,
                })
                .ok_or_else(|| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "finalized operation lost its proven terminal evidence",
                    )
                })?;
            let evidence =
                finalized_operation_evidence(evidence, checkpoint_id, history.identity())?;
            ledger = ledger
                .transition_operation(
                    operation_id.0,
                    DurableOperationTransition::new(
                        checkpoint_id,
                        DurableOperationState::Finalized(evidence),
                    ),
                )
                .map_err(runtime_manifest_error)?;
        }
        let lifecycle = working_lifecycle(turn_id, &ledger)?;
        self.publish_turn_terminal(
            checkpoint_id,
            lifecycle,
            history,
            ledger,
            context,
            family,
            &notices,
        )
        .await
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
    async fn publish_turn_terminal(
        &mut self,
        checkpoint_id: CheckpointId,
        lifecycle: RuntimeLifecycle,
        history: CanonicalHistory,
        ledger: RuntimeOperationLedger,
        context: RuntimeContextState,
        family: Option<FamilyArtifact>,
        notice_consumptions: &[u32],
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError> {
        let context = if lifecycle == RuntimeLifecycle::Quiescent {
            context
                .with_occupancy_anchor(self.context_anchor)
                .and_then(|context| context.with_request_anchor(self.request_anchor.clone()))
                .map_err(runtime_manifest_error)?
        } else {
            context
        };
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

    /// Transitions one operation under the class `transition_admission`
    /// derives from its current and target states.
    pub(super) async fn transition_operation(
        &mut self,
        turn_id: TurnId,
        operation_id: OperationId,
        state: DurableOperationState,
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError> {
        let admission = transition_admission(&self.operation_state(operation_id)?, &state);
        self.transition_operation_as(admission, turn_id, operation_id, state)
            .await
    }

    pub(super) async fn transition_operation_as(
        &mut self,
        admission: StoreAdmission,
        turn_id: TurnId,
        operation_id: OperationId,
        state: DurableOperationState,
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError> {
        let checkpoint_id = self.checkpoint_id()?;
        let (history, ledger, context, family) = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            (
                manifest.canonical().history().clone(),
                manifest
                    .ledger()
                    .transition_operation(
                        operation_id.0,
                        DurableOperationTransition::new(checkpoint_id, state),
                    )
                    .map_err(runtime_manifest_error)?,
                manifest.canonical().context().retained_for_exact_resume(),
                manifest.family_artifact().cloned(),
            )
        };
        let lifecycle = working_lifecycle(turn_id, &ledger)?;
        self.publish_checkpoint(
            admission,
            checkpoint_id,
            lifecycle,
            history,
            ledger,
            context,
            family,
        )
        .await
    }

    /// Transitions several operations atomically; the commit is ordinary
    /// only when every transition is a first fence out of `Prepared`.
    pub(super) async fn transition_operations(
        &mut self,
        turn_id: TurnId,
        transitions: Vec<(OperationId, DurableOperationState)>,
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError> {
        let mut admission = StoreAdmission::Ordinary;
        for (operation_id, state) in &transitions {
            if transition_admission(&self.operation_state(*operation_id)?, state)
                != StoreAdmission::Ordinary
            {
                admission = StoreAdmission::Settlement;
            }
        }
        self.transition_operations_as(admission, turn_id, transitions)
            .await
    }

    pub(super) async fn transition_operations_as(
        &mut self,
        admission: StoreAdmission,
        turn_id: TurnId,
        transitions: Vec<(OperationId, DurableOperationState)>,
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError> {
        if transitions.is_empty() {
            return Ok(self.authority.checkpoint());
        }
        let checkpoint_id = self.checkpoint_id()?;
        let (history, ledger, context, family) = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            let mut ledger = manifest.ledger().clone();
            for (operation_id, state) in transitions {
                ledger = ledger
                    .transition_operation(
                        operation_id.0,
                        DurableOperationTransition::new(checkpoint_id, state),
                    )
                    .map_err(runtime_manifest_error)?;
            }
            (
                manifest.canonical().history().clone(),
                ledger,
                manifest.canonical().context().retained_for_exact_resume(),
                manifest.family_artifact().cloned(),
            )
        };
        let lifecycle = working_lifecycle(turn_id, &ledger)?;
        self.publish_checkpoint(
            admission,
            checkpoint_id,
            lifecycle,
            history,
            ledger,
            context,
            family,
        )
        .await
    }

    pub(super) async fn finalize_operation(
        &mut self,
        turn_id: TurnId,
        operation_id: OperationId,
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError> {
        match self.operation_state(operation_id)? {
            DurableOperationState::Completed(evidence)
            | DurableOperationState::ProvenNotDispatched(evidence) => {
                let checkpoint_id = self.checkpoint_id()?;
                let (history, ledger, context, family) = {
                    let authority = self.authority.checkpoint();
                    let manifest = authority.manifest();
                    let history = manifest.canonical().history().clone();
                    let evidence =
                        finalized_operation_evidence(&evidence, checkpoint_id, history.identity())?;
                    let ledger = manifest
                        .ledger()
                        .transition_operation(
                            operation_id.0,
                            DurableOperationTransition::new(
                                checkpoint_id,
                                DurableOperationState::Finalized(evidence),
                            ),
                        )
                        .map_err(runtime_manifest_error)?;
                    (
                        history,
                        ledger,
                        manifest.canonical().context().retained_for_exact_resume(),
                        manifest.family_artifact().cloned(),
                    )
                };
                let lifecycle = working_lifecycle(turn_id, &ledger)?;
                self.publish_turn_terminal(
                    checkpoint_id,
                    lifecycle,
                    history,
                    ledger,
                    context,
                    family,
                    &[],
                )
                .await
            }
            DurableOperationState::Claimed(_)
            | DurableOperationState::DecisionRequired(_)
            | DurableOperationState::ContinuationRequired(_)
            | DurableOperationState::LocatorBound(_)
            | DurableOperationState::OutcomeUnknown(_) => Err(AgentError::new(
                AgentErrorKind::OutcomeUnknown,
                "operation outcome requires exact certified recovery",
            )),
            DurableOperationState::Prepared
            | DurableOperationState::Dispatched
            | DurableOperationState::Finalized(_) => Err(AgentError::new(
                AgentErrorKind::Internal,
                "operation cannot be finalized from its current state",
            )),
        }
    }

    pub(super) async fn append_history<I>(
        &mut self,
        turn_id: TurnId,
        kind: Option<HistorySegmentKind>,
        items: I,
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError>
    where
        I: IntoIterator<Item = ConversationItem>,
    {
        Box::pin(self.append_history_with_notices(turn_id, kind, items, false)).await
    }

    pub(super) async fn append_history_with_notices<I>(
        &mut self,
        turn_id: TurnId,
        kind: Option<HistorySegmentKind>,
        items: I,
        deliver_notices: bool,
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError>
    where
        I: IntoIterator<Item = ConversationItem>,
    {
        let items = items.into_iter().collect::<Vec<_>>();
        let notices = if deliver_notices {
            self.process_notices()?
        } else {
            None
        };
        if items.is_empty() && notices.is_none() {
            return Ok(self.authority.checkpoint());
        }
        let segment = if items.is_empty() {
            None
        } else {
            Some(
                HistorySegment::new(kind.unwrap_or_else(|| history_segment_kind(&items)), items)
                    .map_err(|_| {
                        AgentError::new(
                            AgentErrorKind::Internal,
                            "canonical history rejected an atomic semantic group",
                        )
                    })?,
            )
        };
        let checkpoint_id = self.checkpoint_id()?;
        let mut consumed = Vec::new();
        let (history, ledger, context, family) = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            let mut history = manifest.canonical().history().clone();
            if let Some((notice, numbers)) = notices {
                history = history
                    .append(notice)
                    .map_err(|_| super::notices::notice_history_error())?;
                consumed = numbers;
            }
            if let Some(segment) = segment {
                history = history
                    .append(segment)
                    .map_err(|_| super::notices::notice_history_error())?;
            }
            (
                history,
                manifest.ledger().clone(),
                manifest.canonical().context().retained_for_exact_resume(),
                manifest.family_artifact().cloned(),
            )
        };
        let lifecycle = working_lifecycle(turn_id, &ledger)?;
        let checkpoint = self
            .publish_checkpoint_consuming_jobs(
                StoreAdmission::Ordinary,
                checkpoint_id,
                lifecycle,
                history,
                ledger,
                context,
                family,
                &consumed,
            )
            .await?;
        let point = if kind == Some(HistorySegmentKind::RecentIntent) {
            if self
                .active_root
                .as_ref()
                .is_some_and(|root| root.instruction)
            {
                crate::JobNoticeDelivery::IdleWake
            } else {
                crate::JobNoticeDelivery::Admission
            }
        } else {
            crate::JobNoticeDelivery::Boundary
        };
        self.observe_notice_delivery(turn_id, point, consumed.len());
        Ok(checkpoint)
    }

    pub(super) fn operation_state(
        &self,
        operation_id: OperationId,
    ) -> Result<DurableOperationState, AgentError> {
        self.authority
            .checkpoint()
            .manifest()
            .ledger()
            .operation(operation_id.0)
            .map(|record| record.state().clone())
            .ok_or_else(|| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "operation state referenced an unknown identity",
                )
            })
    }

    pub(super) fn certify_mutating_effect_recovery(&self) -> Result<(), AgentError> {
        self.authority.require_durability()
    }

    /// The decision commit of a round's provider attempt: the operation's
    /// terminal state carrying its outcome record, plus the finalization of
    /// the continuation baton the round ran under.
    pub(super) async fn finish_provider_operation(
        &mut self,
        turn_id: TurnId,
        operation_id: OperationId,
        attempt: Result<&ModelResponse, &AgentError>,
        outcome: &ProviderOutcome,
        continuation: Option<OperationId>,
    ) -> Result<(), AgentError> {
        let mut plan = self.commit_plan()?;
        self.stage_provider_outcome(&mut plan, operation_id, attempt, outcome, continuation)?;
        self.publish_commit_plan(turn_id, plan).await?;
        Ok(())
    }

    /// Adds a round's decision to `plan`: the provider operation's terminal
    /// state carrying `outcome`, and the finalization of the continuation
    /// baton the round ran under. The provider itself is finalized only when
    /// the caller also commits the decision's consequence in `plan`.
    pub(super) fn stage_provider_outcome(
        &self,
        plan: &mut CommitPlan,
        operation_id: OperationId,
        attempt: Result<&ModelResponse, &AgentError>,
        outcome: &ProviderOutcome,
        continuation: Option<OperationId>,
    ) -> Result<(), AgentError> {
        let authority = self.authority.checkpoint();
        let record = authority
            .manifest()
            .ledger()
            .operation(operation_id.0)
            .ok_or_else(|| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "operation finalization referenced an unknown identity",
                )
            })?;
        let terminal = provider_terminal_state(
            attempt,
            outcome,
            matches!(
                record.state(),
                DurableOperationState::Dispatched | DurableOperationState::LocatorBound(_)
            ),
            expected_provider_dispatch(record),
        )?;
        plan.transitions.push((operation_id, terminal));
        if let Some(continuation) = continuation {
            plan.finalize.insert(continuation);
        }
        Ok(())
    }

    pub(super) async fn finalize_hosted_continuation(
        &mut self,
        turn_id: TurnId,
        continuation: Option<OperationId>,
    ) -> Result<(), AgentError> {
        let Some(continuation) = continuation else {
            return Ok(());
        };
        let checkpoint_id = self.checkpoint_id()?;
        let (history, ledger, context, family) = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            let evidence = manifest
                .ledger()
                .operation(continuation.0)
                .and_then(|record| match record.state() {
                    DurableOperationState::ContinuationRequired(evidence) => Some(evidence),
                    _ => None,
                })
                .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
            let finalized = finalized_operation_evidence(
                evidence,
                checkpoint_id,
                manifest.canonical().history().identity(),
            )?;
            (
                manifest.canonical().history().clone(),
                manifest
                    .ledger()
                    .transition_operation(
                        continuation.0,
                        DurableOperationTransition::new(
                            checkpoint_id,
                            DurableOperationState::Finalized(finalized),
                        ),
                    )
                    .map_err(runtime_manifest_error)?,
                manifest.canonical().context().retained_for_exact_resume(),
                manifest.family_artifact().cloned(),
            )
        };
        let lifecycle = working_lifecycle(turn_id, &ledger)?;
        self.publish_checkpoint(
            StoreAdmission::Settlement,
            checkpoint_id,
            lifecycle,
            history,
            ledger,
            context,
            family,
        )
        .await?;
        Ok(())
    }

    pub(super) async fn finish_effect_decision_operation(
        &mut self,
        turn_id: TurnId,
        operation_id: OperationId,
        binding: Digest,
        resolution: EffectDecisionResolution,
        decision_recorded: bool,
    ) -> Result<(), AgentError> {
        let evidence = runtime_component(
            RuntimeComponentKind::Operation,
            serde_json::to_vec(&("plakous-agent/decision-outcome-v2", binding, resolution))
                .map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "decision outcome evidence could not be canonicalized",
                    )
                })?,
        )?;
        let mut plan = self.commit_plan()?;
        plan.transitions.push((
            operation_id,
            if decision_recorded {
                DurableOperationState::Completed(evidence)
            } else {
                DurableOperationState::ProvenNotDispatched(evidence)
            },
        ));
        plan.finalize.insert(operation_id);
        self.publish_commit_plan(turn_id, plan).await?;
        Ok(())
    }
}

/// How a turn ends in its consequence commit.
pub(super) enum TurnConsequence<'a> {
    /// The final response, committed with its artifact transition.
    Completed {
        response: &'a ModelResponse,
        artifact: ArtifactTransition,
    },
    /// A cancellation or failure; an `OutcomeUnknown` error records
    /// `RecoveryRequired`.
    Ended(&'a AgentError),
}

/// One atomic successor under construction: every decision commits with its
/// consequences in exactly one checkpoint CAS (spec 01 §3.5).
pub(super) struct CommitPlan {
    pub(super) checkpoint_id: CheckpointId,
    /// Ledger transitions applied in order before anything is finalized.
    pub(super) transitions: Vec<(OperationId, DurableOperationState)>,
    /// Records appended to the ledger after the transitions.
    pub(super) appended: Vec<DurableOperationRecord>,
    /// Operations finalized against the committed history.
    pub(super) finalize: BTreeSet<OperationId>,
    /// Also finalize every same-turn record still owing finalization; any
    /// unresolved same-turn record refuses the commit.
    pub(super) finalize_turn: bool,
    /// Drops the trailing segment, which the caller has proven is this
    /// root's own unanswered prompt, before appending.
    pub(super) withhold_prompt: bool,
    pub(super) segments: Vec<HistorySegment>,
    pub(super) artifact: ArtifactTransition,
    /// Process notices the appended history consumes.
    pub(super) notices: Vec<u32>,
}

/// Every record of `turn_id` that is settled but not yet finalized. A
/// record whose outcome is not settled cannot be closed by a turn's
/// consequence commit.
fn settled_turn_operations(
    ledger: &RuntimeOperationLedger,
    turn_id: TurnId,
) -> Result<Vec<OperationId>, AgentError> {
    let mut operations = Vec::new();
    for record in ledger.operations() {
        if operation_turn_id(record) != Some(turn_id) {
            continue;
        }
        match record.state() {
            DurableOperationState::Completed(_) | DurableOperationState::ProvenNotDispatched(_) => {
                operations.push(OperationId(record.operation_id()));
            }
            DurableOperationState::Prepared
            | DurableOperationState::DecisionRequired(_)
            | DurableOperationState::ContinuationRequired(_)
            | DurableOperationState::Claimed(_)
            | DurableOperationState::Dispatched
            | DurableOperationState::LocatorBound(_)
            | DurableOperationState::OutcomeUnknown(_) => {
                return Err(AgentError::new(
                    AgentErrorKind::OutcomeUnknown,
                    "terminal commit has an unresolved durable operation",
                ));
            }
            DurableOperationState::Finalized(_) => {}
        }
    }
    Ok(operations)
}

/// Whether the terminal commit of a turn that ended with `error` keeps the
/// family artifact. A context-capacity failure never started a new provider
/// round, so the artifact stays; any other failure clears it. Shared by the
/// live terminal commit and recovery's finish row.
pub(in crate::runtime) fn failure_preserves_artifact(error: &AgentError) -> bool {
    error
        .model_error()
        .and_then(ModelError::context_capacity)
        .is_some()
}

/// What the terminal commit of a cancelled or failed root adds after its
/// prompt.
pub(in crate::runtime) enum TerminalPromptTail {
    Unchanged,
    Note(ConversationItem),
    /// Replaces the trailing prompt with the note.
    WithholdPrompt(ConversationItem),
}

/// The tail the terminal commit of a cancelled or failed turn appends after
/// its prompt, shared by the live terminal commit and recovery's finish row
/// (spec 01 §5.5) so both publish the same consequence. `withhold` is the
/// measurement a `FailTurn` decision recorded; `trailing` the turn's prompt
/// when it is still the last history group.
pub(in crate::runtime) fn failure_prompt_tail(
    turn_id: TurnId,
    error: &AgentError,
    withhold: bool,
    trailing: Option<(usize, Provenance)>,
) -> Result<TerminalPromptTail, AgentError> {
    match error.kind() {
        AgentErrorKind::OutcomeUnknown => return Ok(TerminalPromptTail::Unchanged),
        AgentErrorKind::Cancelled => {
            return Ok(TerminalPromptTail::Note(cancelled_prompt_note(turn_id)?));
        }
        _ => {}
    }
    if withhold && let Some((bytes, provenance)) = trailing {
        return Ok(TerminalPromptTail::WithholdPrompt(withheld_prompt_note(
            turn_id,
            bytes,
            provenance == Provenance::UserInput,
        )?));
    }
    Ok(TerminalPromptTail::Note(failed_prompt_note(
        turn_id, error,
    )?))
}

/// The turn's prompt, when it is exactly the last history group: the item
/// `prompt` names, or for a recovered head that no longer knows it, the one
/// user item of a trailing recent-intent group.
pub(in crate::runtime) fn trailing_prompt(
    history: &CanonicalHistory,
    prompt: Option<EntryId>,
) -> Option<(usize, Provenance)> {
    let last = history.segments().iter().last()?;
    match last.items() {
        [item]
            if prompt.map_or(
                last.kind() == HistorySegmentKind::RecentIntent && item.actor() == Actor::User,
                |prompt| item.id() == prompt,
            ) =>
        {
            Some((
                item.blocks()
                    .iter()
                    .map(|block| match block {
                        ContentBlock::Text(text) => text.text().len(),
                        _ => 0,
                    })
                    .sum(),
                item.provenance(),
            ))
        }
        _ => None,
    }
}

impl TerminalPromptTail {
    /// Whether the terminal commit drops the trailing prompt, and the segment
    /// it appends.
    ///
    /// A note after an unanswered prompt is conversation: it opens no root
    /// turn. A withheld prompt's note takes the prompt's place as the turn's
    /// recent intent, so the turn it stands for still counts (spec 01 §10.6).
    pub(in crate::runtime) fn into_commit(
        self,
    ) -> Result<(bool, Option<HistorySegment>), AgentError> {
        let (withhold_prompt, kind, note) = match self {
            Self::Unchanged => return Ok((false, None)),
            Self::Note(note) => (false, HistorySegmentKind::Conversation, note),
            Self::WithholdPrompt(note) => (true, HistorySegmentKind::RecentIntent, note),
        };
        let segment = HistorySegment::new(kind, vec![note]).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "terminal prompt note is not a valid history group",
            )
        })?;
        Ok((withhold_prompt, Some(segment)))
    }
}

/// The harness note a cancelled turn leaves after its unanswered prompt.
///
/// The prompt stays in canonical history, which only appends. Without this
/// the next turn's model reads it as still pending and answers it alongside
/// the new request.
/// The prepared call and attempt a provider dispatch rejection must match.
fn expected_provider_dispatch(record: &DurableOperationRecord) -> Option<(Digest, AttemptId)> {
    match record.kind() {
        DurableOperationKind::Provider { attempt_id, .. } => {
            serde_json::from_slice::<PreparedCallRecord>(record.request().bytes())
                .ok()
                .map(|prepared| (prepared.identity(), *attempt_id))
        }
        _ => None,
    }
}

fn cancelled_prompt_note(turn_id: TurnId) -> Result<ConversationItem, AgentError> {
    turn_note(
        turn_id,
        b"plakous-agent/cancelled-prompt-note-v1",
        "the user cancelled the request above before it was answered. Do not \
         continue or act on it unless the user asks for it again."
            .to_owned(),
    )
}

/// The harness note a failed turn leaves after its unanswered prompt. It names
/// the error class only: the diagnostic may quote provider or tool text.
fn failed_prompt_note(turn_id: TurnId, error: &AgentError) -> Result<ConversationItem, AgentError> {
    let class = error
        .model_kind()
        .map_or_else(|| format!("{:?}", error.kind()), |kind| format!("{kind:?}"));
    turn_note(
        turn_id,
        b"plakous-agent/failed-prompt-note-v1",
        format!(
            "the turn handling the request above failed ({class}) before it was answered. \
             Tool results above are what actually ran. Do not continue or act on the request \
             unless the user asks for it again."
        ),
    )
}

/// The note that stands where a prompt too large for the model's window was.
/// `person` is whether a person submitted that prompt (not a host wake).
fn withheld_prompt_note(
    turn_id: TurnId,
    bytes: usize,
    person: bool,
) -> Result<ConversationItem, AgentError> {
    let entry = withheld_prompt_entry_id(turn_id, person).ok_or_else(turn_note_failed)?;
    turn_note_with_entry(
        entry,
        format!(
            "a user message of {bytes} bytes was withheld here: it did not fit the model's \
             context window, so it was never answered. Do not guess at its content; the user \
             can send it again in a smaller form."
        ),
    )
}

const WITHHELD_PROMPT_NOTE_DOMAIN: &[u8] = b"plakous-agent/withheld-prompt-note-v2";
const WITHHELD_WAKE_NOTE_DOMAIN: &[u8] = b"plakous-agent/withheld-wake-note-v1";
/// The leading bytes of a withheld person's prompt's note identity. The rest
/// is derived from the turn, as for every turn note.
const WITHHELD_PROMPT_ENTRY_TAG: [u8; 6] = *b"plkwhp";

/// The entry identity of the note that replaced `turn_id`'s withheld prompt;
/// `person` as for `withheld_prompt_note`.
///
/// A person's prompt's note is recognisable from its identity alone
/// (`is_withheld_prompt_entry`), so the turn it stands for still counts as a
/// person's without reading its text.
pub(in crate::runtime) fn withheld_prompt_entry_id(
    turn_id: TurnId,
    person: bool,
) -> Option<EntryId> {
    if !person {
        return turn_note_entry_id(turn_id, WITHHELD_WAKE_NOTE_DOMAIN);
    }
    let mut bytes = turn_note_bytes(turn_id, WITHHELD_PROMPT_NOTE_DOMAIN)?;
    bytes[..WITHHELD_PROMPT_ENTRY_TAG.len()].copy_from_slice(&WITHHELD_PROMPT_ENTRY_TAG);
    EntryId::try_from(uuid::Uuid::from_bytes(bytes)).ok()
}

/// Whether `entry` is the note that replaced a person's withheld prompt.
pub(in crate::runtime) fn is_withheld_prompt_entry(entry: EntryId) -> bool {
    let uuid = entry.as_uuid();
    uuid.get_version_num() == 8 && uuid.as_bytes().starts_with(&WITHHELD_PROMPT_ENTRY_TAG)
}

pub(super) fn turn_note_entry_id(turn_id: TurnId, domain: &[u8]) -> Option<EntryId> {
    EntryId::try_from(uuid::Uuid::from_bytes(turn_note_bytes(turn_id, domain)?)).ok()
}

/// A version-8 identity derived from `turn_id` and `domain`.
fn turn_note_bytes(turn_id: TurnId, domain: &[u8]) -> Option<[u8; 16]> {
    let digest = Digest::sha256(
        plakous_model::DigestDomain::ProviderOperation,
        2,
        &[domain, turn_id.as_uuid().as_bytes()],
    )
    .ok()?;
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Some(bytes)
}

/// One harness note per turn and `domain`. `User` with
/// `Provenance::Instruction` is the harness spelling every wire can carry (see
/// `abandoned_turn_segment`), and the entry id is derived from the turn so a
/// retried terminal commit adds it once.
fn turn_note(turn_id: TurnId, domain: &[u8], text: String) -> Result<ConversationItem, AgentError> {
    let entry = turn_note_entry_id(turn_id, domain).ok_or_else(turn_note_failed)?;
    turn_note_with_entry(entry, text)
}

fn turn_note_with_entry(entry: EntryId, text: String) -> Result<ConversationItem, AgentError> {
    let text = plakous_model::TextBlock::new(text, None).map_err(|_| turn_note_failed())?;
    ConversationItem::new(
        entry,
        Actor::User,
        vec![ContentBlock::Text(text)],
        Provenance::Instruction,
    )
    .map_err(|_| turn_note_failed())
}

fn turn_note_failed() -> AgentError {
    AgentError::new(
        AgentErrorKind::Internal,
        "terminal turn note could not be built",
    )
}

/// Store admission of one operation transition. Only the first fence out of
/// `Prepared` (a claim, a prepared locator, or the dispatch itself) starts new
/// work: a refusal there leaves the operation `Prepared`, which the turn's
/// settlement commit records as proven not dispatched. Every later step of an
/// operation that already crossed a fence records work that has begun.
pub(super) fn transition_admission(
    current: &DurableOperationState,
    target: &DurableOperationState,
) -> StoreAdmission {
    match (current, target) {
        (
            DurableOperationState::Prepared,
            DurableOperationState::Claimed(_)
            | DurableOperationState::LocatorBound(_)
            | DurableOperationState::Dispatched,
        ) => StoreAdmission::Ordinary,
        _ => StoreAdmission::Settlement,
    }
}

impl Driver {
    pub(super) fn observe_notice_delivery(
        &self,
        turn_id: TurnId,
        point: crate::JobNoticeDelivery,
        count: usize,
    ) {
        if count != 0 {
            self.emit_telemetry(
                self.telemetry_context.with_turn(turn_id),
                crate::TelemetryObservationKind::JobNoticesDelivered {
                    point,
                    count: u32::try_from(count).unwrap_or(u32::MAX),
                },
            );
        }
    }
}

#[cfg(test)]
mod terminal_note_turn_position_tests {
    //! A note the driver leaves after a cancelled, failed or withheld prompt,
    //! or inside a running turn, is not a root turn of its own.
    use super::{
        TerminalPromptTail, cancelled_prompt_note, failed_prompt_note, turn_note_entry_id,
        withheld_prompt_note,
    };
    use crate::runtime::driver::turn_position::turn_position;
    use crate::{AgentError, AgentErrorKind};
    use plakous_model::{
        Actor, CanonicalHistory, ContentBlock, ConversationItem, EntryId, HistorySegment,
        HistorySegmentKind, Provenance, TextBlock, TurnId, TurnPosition,
    };

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn turn(n: u128) -> Result<TurnId, Box<dyn std::error::Error>> {
        Ok(TurnId::try_from(uuid::Uuid::from_u128(0x7000 + n))?)
    }

    fn text(
        n: u128,
        actor: Actor,
        provenance: Provenance,
        body: &str,
    ) -> Result<ConversationItem, Box<dyn std::error::Error>> {
        Ok(ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(n))?,
            actor,
            vec![ContentBlock::Text(TextBlock::new(body, None)?)],
            provenance,
        )?)
    }

    fn opening(
        n: u128,
        provenance: Provenance,
    ) -> Result<HistorySegment, Box<dyn std::error::Error>> {
        Ok(HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(n, Actor::User, provenance, "do the thing")?],
        )?)
    }

    fn prompt(n: u128) -> Result<HistorySegment, Box<dyn std::error::Error>> {
        opening(n, Provenance::UserInput)
    }

    fn reply(n: u128) -> Result<HistorySegment, Box<dyn std::error::Error>> {
        Ok(HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![text(n, Actor::Assistant, Provenance::ModelOutput, "done")?],
        )?)
    }

    /// `history` with the terminal commit of `tail` applied, exactly as
    /// `fail_turn` publishes it.
    fn terminal(
        mut segments: Vec<HistorySegment>,
        tail: TerminalPromptTail,
    ) -> Result<Vec<HistorySegment>, Box<dyn std::error::Error>> {
        let (withhold_prompt, segment) = tail.into_commit()?;
        if withhold_prompt {
            segments.pop();
        }
        segments.extend(segment);
        Ok(segments)
    }

    #[test]
    fn a_cancelled_turn_note_does_not_open_a_turn() -> TestResult {
        let mut segments = terminal(
            vec![prompt(1)?, reply(2)?, prompt(3)?],
            TerminalPromptTail::Note(cancelled_prompt_note(turn(2)?)?),
        )?;
        segments.push(prompt(5)?);
        // Three typed prompts, three root turns.
        assert_eq!(
            turn_position(&CanonicalHistory::new(segments)?),
            TurnPosition::new(3, 3)
        );
        Ok(())
    }

    #[test]
    fn a_failed_turn_note_does_not_open_a_turn() -> TestResult {
        let error = AgentError::new(AgentErrorKind::Internal, "failed");
        let mut segments = terminal(
            vec![prompt(1)?],
            TerminalPromptTail::Note(failed_prompt_note(turn(1)?, &error)?),
        )?;
        segments.push(prompt(5)?);
        assert_eq!(
            turn_position(&CanonicalHistory::new(segments)?),
            TurnPosition::new(2, 2)
        );
        Ok(())
    }

    #[test]
    fn a_withheld_prompt_still_counts_as_a_typed_prompt() -> TestResult {
        // The note replaces the prompt; the person still submitted it.
        let mut segments = terminal(
            vec![prompt(1)?, reply(2)?, prompt(3)?],
            TerminalPromptTail::WithholdPrompt(withheld_prompt_note(turn(2)?, 4096, true)?),
        )?;
        let note = segments.last().ok_or("note")?;
        assert_eq!(note.kind(), HistorySegmentKind::RecentIntent);
        assert_eq!(note.items()[0].provenance(), Provenance::Instruction);
        segments.push(prompt(5)?);
        assert_eq!(
            turn_position(&CanonicalHistory::new(segments)?),
            TurnPosition::new(3, 3)
        );
        Ok(())
    }

    #[test]
    fn a_withheld_wake_still_counts_as_a_wake() -> TestResult {
        let mut segments = terminal(
            vec![prompt(1)?, reply(2)?, opening(3, Provenance::Instruction)?],
            TerminalPromptTail::WithholdPrompt(withheld_prompt_note(turn(2)?, 4096, false)?),
        )?;
        segments.push(prompt(5)?);
        assert_eq!(
            turn_position(&CanonicalHistory::new(segments)?),
            TurnPosition::new(2, 3)
        );
        Ok(())
    }

    #[test]
    fn content_filter_guidance_stays_in_its_turn() -> TestResult {
        // Exactly the item `retry_content_filtered_incomplete` appends, with
        // the kind its `None` resolves to.
        let entry = turn_note_entry_id(turn(1)?, b"plakous-agent/content-filter-guidance-v1")
            .ok_or("entry")?;
        let guidance = ConversationItem::new(
            entry,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("rephrase", None)?)],
            Provenance::Instruction,
        )?;
        let history = CanonicalHistory::new(vec![
            prompt(1)?,
            HistorySegment::new(
                crate::runtime::checkpoint::identity::history_segment_kind(std::slice::from_ref(
                    &guidance,
                )),
                vec![guidance],
            )?,
        ])?;
        // The retry round of the first turn is still turn 1.
        assert_eq!(turn_position(&history), TurnPosition::new(1, 1));
        Ok(())
    }
}
