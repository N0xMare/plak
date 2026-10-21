//! Drive root turns and resume admitted continuations.

use crate::runtime::ArtifactTransition;
use crate::runtime::agent::context_overflow_remedy;
use crate::runtime::checkpoint::identity::history_segment_kind;
use crate::runtime::compaction::request::apply_completed_model_round_occupancy;
use crate::runtime::continuation::{
    AwaitEffectDecisionV1, AwaitServerPauseV1, AwaitToolBatchV2, ContinuationBindingReference,
    ContinuationRoot, ContinuationSteering, ContinuationToolSlot, DurableAgentConfig,
    RestoredContinuationFrame, RestoredHostedFrame, RuntimeContinuation, ToolBatchContinuation,
    runtime_component, runtime_manifest_error,
};
use crate::runtime::control::{effect_intent_bytes, provider_artifact_transition};
use crate::runtime::driver::ToolDispatchOrigin;
use crate::runtime::driver::execution::{
    canonicalize_observed_tool_execution, publish_observation_clip,
};
use crate::runtime::driver::model_cleanup::resolve_model_operation;
use crate::runtime::driver::operations::{CommitPlan, TurnConsequence};
use crate::runtime::driver::{
    AdmittedToolCall, AdmittedToolExecution, AuthorizationOutcome, Driver,
    DurableEffectIntentEnvelope, ToolAdmission, ToolBatchResult,
};
use crate::runtime::operation_record::{
    DecidedRemedy, FailTurnShape, ProviderAttemptResult, ProviderOutcome, RoundDecision,
    canonical_tool_operation_binding, operation_turn_id,
};
use crate::runtime::round_classification::{
    HostedRoundDisposition, RoundClassificationError, RoundClassificationInput, classify_round,
    output_limit_retry_eligible, round_failure_action,
};
use crate::runtime::terminal::{
    DurableAgentError, actor_panic_error, cancelled_error, remove_known_turn, turn_lifecycle_error,
};
use crate::runtime::{
    DriverRestoredStartup, OperationId, PromptCommand, QueuedSteering, RestoredEffectDecision,
    TerminalProposal, ToolExecutionResult, TurnResult,
};
use crate::tool_runtime::PendingEffectDecision;
use crate::{
    AgentError, AgentErrorKind, AgentEventKind, DurableOperationKind, DurableOperationRecord,
    DurableOperationState, DurableOperationTransition, EventContext, RuntimeComponentKind,
    SessionStoreError, StoreAdmission, ToolAuditIdentity,
};
use futures_util::FutureExt;
use plakous_model::{
    Actor, AttemptId, CallId, ContentBlock, ConversationItem, Digest, EntryId, HistorySegment,
    HistorySegmentKind, ModelError, ModelErrorKind, ModelErrorPhase, ModelOperationRoot,
    ModelRequest, ModelResponse, ModelRetryRisk, ModelRetryability, Provenance, TextBlock,
    ToolCallBlock, TurnId, Usage, ValidatedRequestPlan, WorkingProjection,
};
use plakous_tools::{ToolInvocation, ToolProjectionReceiptV2};
use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// The note the harness injects when a turn reaches its model-operation limit.
///
/// It is the visible marker in the transcript and the JSONL: a reader sees why
/// the assistant stopped exploring, and the model is told plainly that the only
/// thing left to do is answer. The round that follows it carries
/// `ToolChoice::None`, so the instruction is enforced on the wire rather than
/// merely requested.
pub const OPERATION_BUDGET_WRAP_UP_NOTE: &str = concat!(
    "[harness] This turn has used its entire model-operation budget, so no ",
    "further tool calls are possible. Answer now with your findings so far, ",
    "and say plainly what you were unable to finish."
);

/// The note the harness injects after discarding a response that reached its
/// output bound before completion.
///
/// No truncated call ran and the response is not in the conversation,
/// so without this the model would not know its last attempt failed, and would
/// most likely repeat it. It names the prepared wire maximum when the provider
/// recorded one, and otherwise the request's product bound, so the model can
/// size its retry.
pub(super) fn output_limit_retry_note(bound: Option<NonZeroU32>) -> String {
    let limit = bound.map_or_else(
        || OUTPUT_LIMIT_UNKNOWN.to_owned(),
        |bound| format!("{OUTPUT_LIMIT_KNOWN_HEAD}{bound}{OUTPUT_LIMIT_KNOWN_TAIL}"),
    );
    format!("{OUTPUT_LIMIT_RETRY_HEAD}{limit}{OUTPUT_LIMIT_RETRY_TAIL}")
}

const OUTPUT_LIMIT_RETRY_HEAD: &str = "[harness] Your previous response reached ";
const OUTPUT_LIMIT_RETRY_TAIL: &str = concat!(
    " before completion, so the response was discarded and no tool call ran. ",
    "Retry with a shorter answer or less output per call: split a large file or ",
    "patch into several smaller writes or edits, or reason more briefly."
);
const OUTPUT_LIMIT_UNKNOWN: &str = "the provider's output limit";
const OUTPUT_LIMIT_KNOWN_HEAD: &str = "its output limit of ";
const OUTPUT_LIMIT_KNOWN_TAIL: &str = " tokens";

/// True for exactly the text of a note this driver appends to a running turn.
///
/// Those notes inherit the turn's provenance; in an operator turn they are
/// `UserInput`, and only their text tells them from what the operator typed.
pub(super) fn is_turn_harness_note(text: &str) -> bool {
    text == OPERATION_BUDGET_WRAP_UP_NOTE || is_output_limit_retry_note(text)
}

fn is_output_limit_retry_note(text: &str) -> bool {
    let Some(limit) = text
        .strip_prefix(OUTPUT_LIMIT_RETRY_HEAD)
        .and_then(|rest| rest.strip_suffix(OUTPUT_LIMIT_RETRY_TAIL))
    else {
        return false;
    };
    limit == OUTPUT_LIMIT_UNKNOWN
        || limit
            .strip_prefix(OUTPUT_LIMIT_KNOWN_HEAD)
            .and_then(|rest| rest.strip_suffix(OUTPUT_LIMIT_KNOWN_TAIL))
            .and_then(|bound| bound.parse::<NonZeroU32>().ok())
            .is_some_and(|bound| output_limit_retry_note(Some(bound)) == text)
}

/// The history segment a completed response commits, if it has output.
fn response_segment(response: &ModelResponse) -> Result<Option<HistorySegment>, AgentError> {
    if response.output().is_empty() {
        return Ok(None);
    }
    HistorySegment::new(
        history_segment_kind(response.output()),
        response.output().to_vec(),
    )
    .map(Some)
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Internal,
            "canonical history rejected an atomic semantic group",
        )
    })
}

/// The retry a settled failed attempt leads to.
enum AttemptRetry {
    ContentFilter,
    OutputLimit,
    OverflowFold,
}

/// How a round without customer tools continues the turn.
enum SettledRound {
    /// The loop resumes, holding the hosted continuation operation, if any.
    Continue(Option<OperationId>),
    Finished(Box<TurnResult>),
}

/// How a failed provider attempt continues the turn.
enum AttemptFailure {
    Retry(RoundDecision),
    EndsTurn(AgentError),
}

impl Driver {
    /// The decision on a round's provider attempt that ended in `error`,
    /// taken before anything acts on it (spec 01 §5.5): a retry or an
    /// overflow fold the budgets still allow, or the failure that ends the
    /// turn. The budgets are the only authority for each guard.
    fn failed_attempt_decision(
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
    async fn fail_turn_decision(
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
    fn provider_outcome(
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
    #[allow(
        clippy::too_many_arguments,
        reason = "the decision commit binds the exact attempt, its usage, decision and baton"
    )]
    async fn record_provider_decision(
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
    async fn commit_retry(
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
    fn content_filter_guidance(
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
    async fn await_content_filter_retry(
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

    pub(super) async fn run_restored_startup(
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
    pub(super) async fn run_admitted_turn(
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

    #[allow(
        clippy::too_many_lines,
        reason = "fenced refresh, ambiguous-commit recovery, and receipt hydration form one ordered recovery transaction"
    )]
    async fn run_turn(&mut self, command: &PromptCommand) -> Result<TurnResult, AgentError> {
        self.noticed_job_terminals.clear();
        self.observed_job_terminals.clear();
        self.run_in_memory_turn(command).await
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the in-memory loop keeps continuation admission and turn transitions auditable"
    )]
    async fn run_in_memory_turn(
        &mut self,
        command: &PromptCommand,
    ) -> Result<TurnResult, AgentError> {
        self.run_turn_from_state(command, true, 0, None).await
    }

    /// A `Continue` round in one commit: the provider's outcome record and the
    /// response's history, finalized together.
    async fn commit_continue_round(
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
    async fn commit_hosted_round(
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
    fn stage_hosted_continuation(
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
    async fn append_harness_note(
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

    fn harness_note(
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
    #[allow(
        clippy::too_many_arguments,
        reason = "the retry commit binds the exact attempt, its usage, decision and baton"
    )]
    async fn commit_output_limit_retry(
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

    fn continuation_root(command: &PromptCommand) -> ContinuationRoot {
        ContinuationRoot {
            command_id: command.command_id.as_str().to_owned(),
            turn_id: command.turn_id,
            follow_up: command.follow_up,
            operation_id: command.operation_id,
            binding: command.binding,
            max_model_rounds: command.max_model_rounds,
        }
    }

    fn continuation_bindings(
        &self,
    ) -> (ContinuationBindingReference, ContinuationBindingReference) {
        let authority = self.authority.checkpoint();
        let manifest = authority.manifest();
        (
            ContinuationBindingReference::from(manifest.binding().admission().reference()),
            ContinuationBindingReference::from(manifest.tools().policy()),
        )
    }

    async fn run_turn_from_state(
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
        loop {
            if command.cancellation.is_cancelled() || self.shutdown.is_cancelled() {
                command.cancellation.cancel();
                return Err(cancelled_error());
            }
            self.reresolve_expired_model(command).await?;
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
            let attempt_id = self.attempt_id()?;
            let request = self.model_request(command, attempt_id, over_budget)?;
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
            self.promote_tool_exchange_then_maybe_compact(
                command,
                attempt_id,
                batch,
                completed_operations,
                provider_artifact_transition(&provider_plan, &response),
            )
            .await?;
            let _ = self.apply_steering(command).await?;
            command.shared.begin_preparing()?;
        }
    }

    /// Appends the turn's prompt to history after any pre-prompt compaction.
    async fn append_turn_prompt(&mut self, command: &PromptCommand) -> Result<(), AgentError> {
        self.maybe_compact_before_turn_prompt(command).await?;
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

    /// Settles an attempt that ended with a definite error: its decision commits
    /// first, then the round is retried or the turn fails.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "the failed attempt's decision, retry commit and overflow fold stay in one ordered body"
    )]
    async fn settle_failed_attempt(
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
    async fn settle_model_round(
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

    /// The restorable frame of a round paused at its customer tool batch.
    #[allow(
        clippy::too_many_arguments,
        reason = "the frame records every round input the restored batch needs"
    )]
    fn tool_batch_continuation(
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

    /// The plan a restored frame's provider response was validated under,
    /// checked against this driver's configuration through its stable
    /// identities only: a re-resolved or freshly reopened driver observes the
    /// descriptor afresh.
    fn restore_frame_plan(
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

    async fn resume_restored_turn(
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
        self.promote_tool_exchange_then_maybe_compact(
            command,
            frame.attempt_id,
            batch,
            operations,
            provider_artifact_transition(&provider_plan, &frame.provider_response),
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

    async fn resume_hosted_server_pause(
        &mut self,
        command: &PromptCommand,
        restored: RestoredHostedFrame<AwaitServerPauseV1>,
    ) -> Result<TurnResult, AgentError> {
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
        command.shared.continue_after_model()?;
        let tool_calls = usize::try_from(frame.tool_calls)
            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
        self.run_turn_from_state(command, false, tool_calls, Some(continuation_operation))
            .await
    }

    async fn resume_hosted_tool_batch(
        &mut self,
        command: &PromptCommand,
        restored: RestoredHostedFrame<AwaitToolBatchV2>,
    ) -> Result<TurnResult, AgentError> {
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
                &command.shared.events,
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
                            &command.shared.events,
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
        self.promote_tool_exchange_then_maybe_compact(
            command,
            frame.attempt_id,
            batch,
            operations,
            provider_artifact_transition(&plan, &frame.provider_response),
        )
        .await?;
        let _ = self.apply_steering(command).await?;
        command.shared.begin_preparing()?;
        let tool_calls = usize::try_from(frame.tool_calls)
            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?
            .saturating_add(dispatched);
        self.run_turn_from_state(command, false, tool_calls, Some(continuation_operation))
            .await
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the restored batch carries the complete persisted pause tuple"
    )]
    async fn resume_restored_batch(
        &mut self,
        command: &PromptCommand,
        frame: &AwaitEffectDecisionV1,
        invocation: ToolInvocation,
        projection_receipt: Option<ToolProjectionReceiptV2>,
        pending: PendingEffectDecision,
        provider_plan: &ValidatedRequestPlan,
    ) -> Result<ToolBatchResult, AgentError> {
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
        let authorization = Box::pin(self.drive_restored_effect_decision(
            command,
            pending_call,
            pending,
            frame.decision_operation,
            frame.decision.clone(),
            frame.decision_expires_at,
        ))
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
                        &command.shared.events,
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
    async fn execute_restored_authorization(
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
                    &command.shared.events,
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
            }
            AuthorizationOutcome::Pending(_) => Err(AgentError::new(
                AgentErrorKind::Internal,
                "resolved restored decision remained pending",
            )),
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "restored dispatch preserves the complete persisted operation tuple"
    )]
    pub(super) async fn execute_restored_admitted(
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
    ) -> Result<(), AgentError> {
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
                operations.push(operation_id);
            }
        }
        Ok(())
    }
}
