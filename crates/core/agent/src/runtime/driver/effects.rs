//! Mutating effect admission and exact decision resolution.

use crate::runtime::config::{timestamp_after, timestamp_remaining};
use crate::runtime::continuation::{
    AwaitEffectDecisionV1, ContinuationBindingReference, ContinuationRoot, ContinuationSteering,
    ContinuationToolSlot, DurableAgentConfig, RuntimeContinuation, ToolBatchContinuation,
    runtime_component, runtime_manifest_error,
};
use crate::runtime::control::{command_binding, command_canonical_bytes, event_error};
use crate::runtime::driver::ToolDispatchOrigin;
use crate::runtime::driver::execution::{
    canonicalize_observed_tool_execution, drain_tool_progress, publish_observation_clip,
    publish_tool_progress,
};
use crate::runtime::driver::{
    ActorToolDispatchObserver, AuthorizationOutcome, Driver, MutatingExecution,
    ProgressForwardingGuard, ToolDispatchTelemetry,
};
use crate::runtime::observation::{
    normalized_tool_failure, tool_call_observation, tool_result_content_bytes,
    tool_result_observation_with_subject, validate_pending_effect_decision,
};
use crate::runtime::operation_record::completed_tool_outcome_component;
use crate::runtime::terminal::{cancelled_error, tool_terminal_status};
use crate::runtime::{OperationId, PromptCommand, ToolExecutionResult};
use crate::tool_runtime::{
    ClaimedToolInvocation, EffectAuthorization, PendingEffectDecision, ToolDispatchState,
    ToolExecutionControl,
};
use crate::{
    ActorCommandEffect, ActorCommandReply, ActorControl, AgentError, AgentErrorKind,
    AgentEventKind, CommandAck, ControlError, ControlErrorKind, DurableOperationKind,
    DurableOperationState, EffectDecisionChoice, EffectDecisionRef, EffectDecisionResolution,
    EffectDecisionStatus, EventContext, PendingEffectDecisionState, RuntimeComponentKind,
    StoreAdmission, ToolAuditIdentity, ToolCallerLineageView, TurnCommand, ViewError,
};
use plakous_model::{
    AttemptId, CallId, Digest, EntryId, EventId, Timestamp, ToolCallBlock, WorkingProjection,
};
use plakous_tools::{
    EffectDecision, ProgressSink, ToolInvocation, ToolOutcomeKind, ToolPolicyDenial,
    ToolProjectionReceiptV2, ToolRuntimeError, ToolRuntimeErrorKind,
};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
impl Driver {
    #[allow(
        clippy::too_many_arguments,
        reason = "the arguments are the exact admitted mutating-effect authority tuple"
    )]
    pub(super) async fn execute_mutating_effect(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        operation_id: OperationId,
        invocation: ToolInvocation,
        projection_receipt: Option<ToolProjectionReceiptV2>,
        claimed: ClaimedToolInvocation,
        call: &ToolCallBlock,
        audit_identity: &ToolAuditIdentity,
        result_entry_id: EntryId,
        origin: ToolDispatchOrigin,
        parent_call_id: Option<CallId>,
    ) -> Result<MutatingExecution, AgentError> {
        self.authority.check_heartbeat()?;
        self.transition_operation(
            command.turn_id,
            operation_id,
            DurableOperationState::Dispatched,
        )
        .await?;
        self.authority.check_heartbeat()?;
        let call_id = call.call_id();
        let lineage = ToolCallerLineageView::from_lineage(
            call.correlation().caller_lineage(),
            parent_call_id,
        );
        let (progress, progress_receiver) = ProgressSink::channel(
            self.config.tool_progress_capacity,
            call_id,
            Arc::clone(&self.clock),
        )
        .map_err(|error| AgentError::from_tool(&error))?;
        let progress_guard = ProgressForwardingGuard(progress.clone());
        let dispatch_witness = Arc::new(AtomicBool::new(false));
        let telemetry_context = self
            .telemetry_context
            .with_turn(command.turn_id)
            .with_attempt(attempt_id)
            .with_call(call_id);
        let observer = Arc::new(ActorToolDispatchObserver::new(
            self.start_authority(),
            command.cancellation.clone(),
            Arc::clone(&command.shared),
            EventContext::call(command.turn_id, attempt_id, call_id),
            audit_identity.clone(),
            lineage.clone(),
            tool_call_observation(call),
            ToolDispatchTelemetry {
                emitter: Arc::clone(&self.telemetry),
                context: telemetry_context,
                clock: Arc::clone(&self.clock),
            },
            Arc::clone(&dispatch_witness),
        ));
        let control = ToolExecutionControl::new(
            command.cancellation.clone(),
            None,
            Arc::clone(&self.clock),
            progress,
        )
        .with_dispatch_observer(observer.clone());
        let (control, _preempt_guard) = if origin == ToolDispatchOrigin::TopLevel {
            let (preempt, guard) = self.dispatch_preempt();
            (control.with_preempt(preempt), Some(guard))
        } else {
            (control, None)
        };
        let tools = self.tools.clone();
        let mut execution = Box::pin(tools.execute_claimed(claimed, control));
        let mut progress_open = true;
        let mut controls_open = true;
        let mut boundary_error = None;
        let report = loop {
            tokio::select! {
                () = self.authority.lease_lost(), if boundary_error.is_none() => {
                    command.cancellation.cancel();
                    boundary_error = Some(self.authority.heartbeat_failure());
                }
                () = command.cancellation.cancelled(), if boundary_error.is_none() => {
                    boundary_error = Some(cancelled_error());
                }
                control = self.control_receiver.recv(), if controls_open => {
                    if let Some(control) = control {
                        Box::pin(self.handle_control(Some(command), control)).await;
                    } else {
                        controls_open = false;
                        command.cancellation.cancel();
                        boundary_error.get_or_insert_with(cancelled_error);
                    }
                }
                progress = progress_receiver.recv(), if progress_open => {
                    match progress {
                        Some(progress) => publish_tool_progress(
                            &command.shared.events,
                            command.turn_id,
                            attempt_id,
                            call_id,
                            audit_identity,
                            &lineage,
                            &progress,
                        ),
                        None => progress_open = false,
                    }
                }
                report = &mut execution => break report,
            }
        };
        progress_guard.close();
        drain_tool_progress(
            &progress_receiver,
            &command.shared.events,
            command.turn_id,
            attempt_id,
            call_id,
            audit_identity,
            &lineage,
        )
        .await;
        let dispatched = matches!(report.state(), ToolDispatchState::Started);
        let outcome_unknown = report
            .outcome()
            .is_some_and(|outcome| outcome.kind() == ToolOutcomeKind::OutcomeUnknown);
        let uncertainty_cause = report
            .uncertainty_cause()
            .map(|error| error.message().to_owned());
        let outcome = report.into_result();
        if let Ok(outcome) = &outcome {
            observer.observe_job_outcome(outcome);
        }
        let process_session_update = outcome
            .as_ref()
            .ok()
            .and_then(plakous_tools::internal::process_session_update);
        let (result, presentation_subject) = match &outcome {
            Ok(outcome) => {
                let raised = self
                    .tool_presentation
                    .raise(projection_receipt.as_ref(), outcome.clone())
                    .map_err(|error| AgentError::from_tool(&error));
                match raised {
                    Ok(raised) => {
                        let presentation_subject = raised.presentation_subject().map(str::to_owned);
                        let result = raised
                            .to_model_result(&invocation)
                            .map_err(|error| AgentError::from_tool(&error));
                        (result, presentation_subject)
                    }
                    Err(error) => (Err(error), None),
                }
            }
            Err(error) => (normalized_tool_failure(call, error), None),
        };
        let output_bytes = result.as_ref().map_or(0, tool_result_content_bytes);
        observer.emit_finished(
            tool_terminal_status(&outcome, self.shutdown.is_cancelled()),
            output_bytes,
        );
        let exact = result.map(|result| {
            ToolExecutionResult::new(result, dispatched)
                .with_presentation_subject(presentation_subject)
        });
        let evidence = completed_tool_outcome_component(&exact, result_entry_id)?;
        let terminal = if !dispatched {
            DurableOperationState::ProvenNotDispatched(evidence)
        } else if outcome_unknown {
            DurableOperationState::OutcomeUnknown(evidence)
        } else {
            DurableOperationState::Completed(evidence)
        };
        self.transition_operation(command.turn_id, operation_id, terminal)
            .await?;
        if let Some(update) = process_session_update {
            self.apply_process_session_update(command.turn_id, update)
                .await?;
        }
        if let Some(error) = observer.take_error() {
            return Err(error);
        }
        if let Some(error) = boundary_error {
            return Err(error);
        }
        if outcome_unknown {
            return Err(AgentError::new(
                AgentErrorKind::OutcomeUnknown,
                uncertainty_cause.map_or_else(
                    || String::from("mutating tool outcome requires exact journal reconciliation"),
                    |cause| {
                        format!(
                            "mutating tool outcome requires exact journal reconciliation: {cause}"
                        )
                    },
                ),
            ));
        }
        let exact = exact?;
        let observation = tool_result_observation_with_subject(
            &exact.result,
            exact.presentation_subject.as_deref(),
        );
        // The mutating-effect path. Every tool that writes comes through here,
        // which is most of what a real turn runs, and it was the one
        // canonicalizing site that still discarded the clip: a backtrace from
        // `clip_tool_result` on a live durable run named this line.
        let (item, clip) = canonicalize_observed_tool_execution(
            result_entry_id,
            exact,
            &mut self.observation_budget,
        )?;
        publish_observation_clip(
            &command.shared.events,
            command.turn_id,
            attempt_id,
            call_id,
            call.exposed_name(),
            clip,
        );
        let _ = command.shared.events.publish(
            EventContext::call(command.turn_id, attempt_id, call_id),
            AgentEventKind::ToolCompleted {
                identity: audit_identity.clone(),
                lineage,
                observation: Some(observation),
            },
        );
        Ok(MutatingExecution { item, dispatched })
    }

    pub(super) async fn authorize_tool(
        &mut self,
        command: &PromptCommand,
        call: &ToolCallBlock,
        invocation: ToolInvocation,
    ) -> Result<AuthorizationOutcome, AgentError> {
        let tools = self.tools.clone();
        let mut authorization = Box::pin(tools.authorize(invocation));
        let authorization = loop {
            tokio::select! {
                () = self.authority.lease_lost() => {
                    command.cancellation.cancel();
                    return Err(self.authority.heartbeat_failure());
                }
                () = command.cancellation.cancelled() => return Err(cancelled_error()),
                () = self.shutdown.cancelled() => {
                    command.cancellation.cancel();
                    return Err(cancelled_error());
                }
                control = self.control_receiver.recv() => {
                    let Some(control) = control else {
                        command.cancellation.cancel();
                        return Err(cancelled_error());
                    };
                    Box::pin(self.handle_control(Some(command), control)).await;
                }
                result = &mut authorization => break result,
            }
        };
        match authorization {
            Ok(EffectAuthorization::Allowed(authorized)) => {
                Ok(AuthorizationOutcome::Authorized(Box::new(authorized)))
            }
            Ok(EffectAuthorization::Denied(denial)) => {
                let error =
                    ToolRuntimeError::new(ToolRuntimeErrorKind::Policy, denial.reason().as_str());
                Ok(AuthorizationOutcome::Rejected(normalized_tool_failure(
                    call, &error,
                )?))
            }
            Ok(EffectAuthorization::DecisionRequired(pending)) => {
                Ok(AuthorizationOutcome::Pending(Box::new(pending)))
            }
            Err(error) => Ok(AuthorizationOutcome::Rejected(normalized_tool_failure(
                call, &error,
            )?)),
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the actor-owned effect-decision state machine remains auditable as one cancellation-safe select loop"
    )]
    pub(super) async fn await_effect_decision(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        call: &ToolCallBlock,
        pending: PendingEffectDecision,
        continuation: &ToolBatchContinuation<'_>,
    ) -> Result<AuthorizationOutcome, AgentError> {
        let audit_identity = ToolAuditIdentity::from_invocation(pending.invocation());
        let prompt = pending.prompt().clone();
        let expires_at = timestamp_after(self.clock.now(), self.config.effect_decision_timeout);
        let binding_digest = pending.binding().digest();
        let decision_id = EventId::generate(self.clock.as_ref(), self.ids.as_ref())
            .map_err(|_| event_error(ViewError::Identity))?;
        let decision_ref = EffectDecisionRef::new(
            decision_id,
            command.turn_id,
            attempt_id,
            call.call_id(),
            binding_digest,
        );
        let operation_id = self.take_operation_id()?;
        let slots = effect_decision_slots(continuation, binding_digest)?;
        // The provider response and source-ordered slots below are the
        // uncommitted exchange.  `WorkingProjection` remains the exact empty
        // model-visible overlay at this pre-promotion barrier; an incomplete
        // tool exchange is intentionally not smuggled in as a history segment.
        let working = WorkingProjection::default();
        let (model_admission_reference, policy_reference) = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            (
                ContinuationBindingReference::from(manifest.binding().admission().reference()),
                ContinuationBindingReference::from(manifest.tools().policy()),
            )
        };
        let continuation =
            RuntimeContinuation::AwaitEffectDecisionV1(Box::new(AwaitEffectDecisionV1 {
                root: ContinuationRoot {
                    command_id: command.command_id.as_str().to_owned(),
                    turn_id: command.turn_id,
                    follow_up: command.follow_up,
                    operation_id: command.operation_id,
                    binding: command.binding,
                    max_model_rounds: command.max_model_rounds,
                },
                config: DurableAgentConfig::from_config(self.config)?,
                budgets: self.turn_budgets,
                tool_calls: u64::try_from(continuation.tool_calls).map_err(|_| {
                    AgentError::new(AgentErrorKind::Internal, "tool call counter overflowed")
                })?,
                usage: command.shared.usage(),
                context_anchor: self.context_anchor,
                request_anchor: self
                    .request_anchor
                    .as_ref()
                    .map(plakous_model::ContextRequestAnchor::identity)
                    .transpose()
                    .map_err(|_| AgentError::from_store(crate::SessionStoreError::corrupt()))?,
                attempt_id,
                provider_operation: continuation.provider_operation,
                provider_request: continuation.provider_plan.request().clone(),
                provider_plan_digest: continuation.provider_plan.digest(),
                provider_response: continuation.provider_response.clone(),
                working,
                steering: self
                    .steering
                    .iter()
                    .map(|queued| ContinuationSteering {
                        content: queued.content.clone(),
                        operation_id: queued.operation_id,
                    })
                    .collect(),
                slots,
                pending_index: u32::try_from(continuation.source_index).map_err(|_| {
                    AgentError::new(AgentErrorKind::Internal, "pending tool index overflowed")
                })?,
                decision_operation: operation_id,
                decision: decision_ref.clone(),
                decision_prompt: prompt.description().as_str().to_owned(),
                decision_expires_at: expires_at,
                dispatch_capability_minted: false,
                hosted_continuation_operation: self
                    .hosted_continuation_operation(command, attempt_id)?,
                observation_budget: self.observation_budget,
                model_admission_reference,
                policy_reference,
            }));
        let continuation_bytes = serde_json::to_vec(&continuation).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "runtime continuation could not be canonically encoded",
            )
        })?;
        self.persist_effect_decision_operation(
            command,
            attempt_id,
            call,
            operation_id,
            binding_digest,
            continuation_bytes,
        )
        .await?;
        let published = match command.shared.events.publish_with(
            EventContext::call(command.turn_id, attempt_id, call.call_id()),
            |_event_id| AgentEventKind::EffectDecisionRequired {
                identity: audit_identity.clone(),
                decision: decision_ref.clone(),
                prompt: Some(prompt),
                expires_at,
            },
        ) {
            Ok(published) => published,
            Err(error) => {
                self.finish_effect_decision_operation(
                    command.turn_id,
                    operation_id,
                    binding_digest,
                    EffectDecisionResolution::Invalidated,
                    false,
                )
                .await?;
                return Err(error);
            }
        };
        let AgentEventKind::EffectDecisionRequired { decision, .. } = published.kind() else {
            return Err(AgentError::new(
                AgentErrorKind::Internal,
                "effect-decision event projection lost its typed payload",
            ));
        };
        let decision_ref = decision.clone();
        Box::pin(self.drive_effect_decision(
            command,
            call,
            pending,
            operation_id,
            decision_ref,
            &audit_identity,
            self.config.effect_decision_timeout,
        ))
        .await
    }

    /// The hosted tool-batch continuation operation that owns this attempt, if any.
    fn hosted_continuation_operation(
        &self,
        command: &PromptCommand,
        attempt_id: AttemptId,
    ) -> Result<Option<OperationId>, AgentError> {
        let authority = self.authority.checkpoint();
        let matches = authority
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .filter(|record| {
                matches!(
                    (record.kind(), record.state()),
                    (
                        DurableOperationKind::ToolBatchContinuation {
                            turn_id,
                            attempt_id: operation_attempt,
                        },
                        DurableOperationState::ContinuationRequired(_)
                    ) if *turn_id == command.turn_id && *operation_attempt == attempt_id
                )
            })
            .map(|record| OperationId(record.operation_id()))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => Ok(None),
            [operation] => Ok(Some(*operation)),
            _ => Err(AgentError::new(
                AgentErrorKind::Internal,
                "effect decision found multiple hosted continuation owners",
            )),
        }
    }

    /// Prepares the effect-decision operation and publishes it as decision-required.
    async fn persist_effect_decision_operation(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        call: &ToolCallBlock,
        operation_id: OperationId,
        binding_digest: Digest,
        continuation_bytes: Vec<u8>,
    ) -> Result<(), AgentError> {
        self.prepare_operation_with_id(
            StoreAdmission::Ordinary,
            command.turn_id,
            operation_id,
            DurableOperationKind::EffectDecision {
                turn_id: command.turn_id,
                attempt_id,
                call_id: call.call_id(),
            },
            binding_digest,
            Some(continuation_bytes.clone()),
            None,
        )
        .await
        .map_err(|error| {
            AgentError::new(
                error.kind(),
                format!("continuation preparation failed: {}", error.message()),
            )
        })?;
        let decision_state = PendingEffectDecisionState::new(runtime_component(
            RuntimeComponentKind::Operation,
            continuation_bytes,
        )?)
        .map_err(runtime_manifest_error)?;
        self.transition_operation(
            command.turn_id,
            operation_id,
            DurableOperationState::DecisionRequired(decision_state),
        )
        .await
        .map_err(|error| {
            AgentError::new(
                error.kind(),
                format!("continuation publication failed: {}", error.message()),
            )
        })?;
        Ok(())
    }

    fn project_resolved_effect_decision(
        call: &ToolCallBlock,
        resolved: Result<EffectAuthorization, ToolRuntimeError>,
    ) -> Result<AuthorizationOutcome, AgentError> {
        match resolved {
            Ok(EffectAuthorization::Allowed(authorized)) => {
                Ok(AuthorizationOutcome::Authorized(Box::new(authorized)))
            }
            Ok(EffectAuthorization::Denied(denial)) => {
                let error =
                    ToolRuntimeError::new(ToolRuntimeErrorKind::Policy, denial.reason().as_str());
                Ok(AuthorizationOutcome::Rejected(normalized_tool_failure(
                    call, &error,
                )?))
            }
            Ok(_) => {
                let error = ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Policy,
                    "resolved decision returned an unsupported authorization state",
                );
                Ok(AuthorizationOutcome::Rejected(normalized_tool_failure(
                    call, &error,
                )?))
            }
            Err(error) => Ok(AuthorizationOutcome::Rejected(normalized_tool_failure(
                call, &error,
            )?)),
        }
    }

    pub(super) async fn drive_restored_effect_decision(
        &mut self,
        command: &PromptCommand,
        call: &ToolCallBlock,
        pending: PendingEffectDecision,
        operation_id: OperationId,
        decision_ref: EffectDecisionRef,
        expires_at: Timestamp,
    ) -> Result<AuthorizationOutcome, AgentError> {
        let audit_identity = ToolAuditIdentity::from_invocation(pending.invocation());
        let remaining = timestamp_remaining(self.clock.now(), expires_at);
        Box::pin(self.drive_effect_decision(
            command,
            call,
            pending,
            operation_id,
            decision_ref,
            &audit_identity,
            remaining,
        ))
        .await
    }

    /// Waits for the operator's answer to one pending effect decision, live or
    /// restored. Every exit settles the decision operation, publishes the
    /// resolution and leaves a tombstone, so later commands naming the
    /// decision are answered the same way on both paths.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "the actor-owned effect-decision state machine remains auditable as one cancellation-safe select loop"
    )]
    async fn drive_effect_decision(
        &mut self,
        command: &PromptCommand,
        call: &ToolCallBlock,
        pending: PendingEffectDecision,
        operation_id: OperationId,
        decision_ref: EffectDecisionRef,
        audit_identity: &ToolAuditIdentity,
        remaining: std::time::Duration,
    ) -> Result<AuthorizationOutcome, AgentError> {
        let binding_digest = decision_ref.binding_digest();
        let mut pending = Some(pending);
        let expiry = tokio::time::sleep(remaining);
        tokio::pin!(expiry);

        loop {
            tokio::select! {
                biased;
                () = self.authority.lease_lost() => {
                    command.cancellation.cancel();
                    drop(pending.take());
                    self.finish_effect_decision_operation(command.turn_id, operation_id, binding_digest, EffectDecisionResolution::Invalidated, false).await?;
                    self.resolve_effect_decision_event(command, decision_ref, audit_identity, EffectDecisionResolution::Invalidated);
                    return Err(self.authority.heartbeat_failure());
                }
                () = self.shutdown.cancelled() => {
                    command.cancellation.cancel();
                    drop(pending.take());
                    self.finish_effect_decision_operation(command.turn_id, operation_id, binding_digest, EffectDecisionResolution::Shutdown, false).await?;
                    self.resolve_effect_decision_event(command, decision_ref, audit_identity, EffectDecisionResolution::Shutdown);
                    return Err(cancelled_error());
                }
                () = command.cancellation.cancelled() => {
                    drop(pending.take());
                    self.finish_effect_decision_operation(command.turn_id, operation_id, binding_digest, EffectDecisionResolution::Cancelled, false).await?;
                    self.resolve_effect_decision_event(command, decision_ref, audit_identity, EffectDecisionResolution::Cancelled);
                    return Err(cancelled_error());
                }
                () = &mut expiry => {
                    let Some(pending) = pending.take() else {
                        return Err(AgentError::new(
                            AgentErrorKind::Internal,
                            "pending effect-decision capability was unavailable",
                        ));
                    };
                    let denial = ToolPolicyDenial::new("tool decision expired before an operator decision");
                    let recorded_decision = EffectDecision::deny(decision_ref.binding_digest(), denial);
                    let resolved = self.tools.resolve_effect_decision(pending, recorded_decision);
                    let resolution = if matches!(&resolved, Ok(EffectAuthorization::Denied(_))) {
                        EffectDecisionResolution::Expired
                    } else {
                        EffectDecisionResolution::Invalidated
                    };
                    self.finish_effect_decision_operation(command.turn_id, operation_id, binding_digest, resolution, true).await?;
                    self.resolve_effect_decision_event(command, decision_ref, audit_identity, resolution);
                    return Self::project_resolved_effect_decision(call, resolved);
                }
                control = self.control_receiver.recv() => {
                    let Some(control) = control else {
                        command.cancellation.cancel();
                        drop(pending.take());
                        self.finish_effect_decision_operation(command.turn_id, operation_id, binding_digest, EffectDecisionResolution::Shutdown, false).await?;
                        self.resolve_effect_decision_event(command, decision_ref, audit_identity, EffectDecisionResolution::Shutdown);
                        return Err(cancelled_error());
                    };
                    match control {
                        ActorControl::RecordJobTerminals { reply } => { let _ = reply.send(Ok(false)); }
                        ActorControl::Command {
                            command: incoming @ TurnCommand::ResolveEffectDecision { .. },
                            reply,
                        } => {
                            let binding = match command_binding(&incoming) {
                                Ok(binding) => binding,
                                Err(error) => {
                                    let response = self.unrecorded_rejection(
                                        incoming.command_id().clone(),
                                        incoming.target_turn_id(),
                                        error,
                                    );
                                    let _ = reply.send(response);
                                    continue;
                                }
                            };
                            let canonical_input = command_canonical_bytes(&incoming);
                            if let Some(recorded) = self.command_ledger.get(incoming.command_id()) {
                                let response = if recorded.binding == binding {
                                    ActorCommandReply {
                                        acknowledgement: CommandAck::duplicate(
                                            incoming.command_id().clone(),
                                            &recorded.acknowledgement,
                                        ),
                                        turn: None,
                                        result: recorded.result.clone(),
                                        effect: recorded.effect,
                                    }
                                } else {
                                    self.unrecorded_rejection(
                                        incoming.command_id().clone(),
                                        incoming.target_turn_id(),
                                        ControlError::new(
                                            ControlErrorKind::IdentityConflict,
                                            "command identity was reused with different canonical content",
                                        ),
                                    )
                                };
                                let _ = reply.send(response);
                                continue;
                            }
                            let TurnCommand::ResolveEffectDecision {
                                command_id,
                                reference: supplied,
                                decision,
                            } = incoming else {
                                unreachable!();
                            };
                            if let Err(error) = validate_pending_effect_decision(
                                supplied.turn_id(),
                                &decision_ref,
                                &supplied,
                            ) {
                                let response = self.record_rejection(
                                    command_id,
                                    binding,
                                    canonical_input,
                                    Some(supplied.turn_id()),
                                    error,
                                ).await;
                                let _ = reply.send(response);
                                continue;
                            }
                            let Some(pending_capability) = pending.take() else {
                                let response = self.record_rejection(
                                    command_id,
                                    binding,
                                    canonical_input,
                                    Some(supplied.turn_id()),
                                    ControlError::new(
                                        ControlErrorKind::Stale,
                                        "tool decision is no longer pending",
                                    ),
                                ).await;
                                let _ = reply.send(response);
                                continue;
                            };
                            let (recorded_decision, status) = match decision {
                                EffectDecisionChoice::Allow(decision_audit_id) => (
                                    EffectDecision::allow(decision_ref.binding_digest(), decision_audit_id),
                                    EffectDecisionStatus::Allowed,
                                ),
                                EffectDecisionChoice::Deny(denial) => (
                                    EffectDecision::deny(decision_ref.binding_digest(), denial),
                                    EffectDecisionStatus::Denied,
                                ),
                            };
                            let (acknowledgement, command_operation) = match self.record_acceptance(
                                command_id.clone(),
                                binding,
                                canonical_input,
                                Some(decision_ref.turn_id()),
                                ActorCommandEffect::EffectDecision(status),
                            ).await {
                                Ok(receipt) => receipt,
                                Err(error) => {
                                    pending = Some(pending_capability);
                                    let response = self.command_persistence_failure(command_id, Some(decision_ref.turn_id()), &error);
                                    let _ = reply.send(response);
                                    continue;
                                }
                            };
                            let resolved = self.tools.resolve_effect_decision(pending_capability, recorded_decision);
                            let resolution = match (&resolved, status) {
                                (Ok(EffectAuthorization::Allowed(_)), EffectDecisionStatus::Allowed) => {
                                    EffectDecisionResolution::Allowed
                                }
                                (Ok(EffectAuthorization::Denied(_)), EffectDecisionStatus::Denied) => {
                                    EffectDecisionResolution::Denied
                                }
                                _ => EffectDecisionResolution::Invalidated,
                            };
                            self.finish_effect_decision_operation(command.turn_id, operation_id, binding_digest, resolution, true).await?;
                            self.complete_command_operation(command.turn_id, command_operation, ActorCommandEffect::EffectDecision(status)).await?;
                            self.resolve_effect_decision_event(command, decision_ref, audit_identity, resolution);
                            let _ = reply.send(ActorCommandReply {
                                acknowledgement,
                                turn: None,
                                result: None,
                                effect: ActorCommandEffect::EffectDecision(status),
                            });
                            return Self::project_resolved_effect_decision(call, resolved);
                        }
                        other @ (ActorControl::Command { .. }
                        | ActorControl::Background { .. }
                        | ActorControl::ContextStatus { .. }
                        | ActorControl::CompactSession { .. }) => {
                            Box::pin(self.handle_control(Some(command), other)).await;
                        }
                    }
                }
            }
        }
    }

    fn resolve_effect_decision_event(
        &mut self,
        command: &PromptCommand,
        decision: EffectDecisionRef,
        audit_identity: &ToolAuditIdentity,
        resolution: EffectDecisionResolution,
    ) {
        let _ = command.shared.events.publish(
            EventContext::call(command.turn_id, decision.attempt_id(), decision.call_id()),
            AgentEventKind::EffectDecisionResolved {
                identity: audit_identity.clone(),
                decision: decision.clone(),
                resolution,
            },
        );
        self.remember_effect_decision(decision, resolution);
    }
}

/// The source-ordered tool slots of a batch paused at its pending effect decision.
fn effect_decision_slots(
    continuation: &ToolBatchContinuation<'_>,
    binding_digest: Digest,
) -> Result<Vec<ContinuationToolSlot>, AgentError> {
    let slots = continuation
        .calls
        .iter()
        .cloned()
        .zip(continuation.result_entry_ids.iter().copied())
        .enumerate()
        .map(
            |(index, (slot_call, result_entry_id))| match index.cmp(&continuation.source_index) {
                std::cmp::Ordering::Less => {
                    let item = continuation.items[index].clone().ok_or_else(|| {
                        AgentError::new(
                            AgentErrorKind::Internal,
                            "effect-decision barrier has an incomplete tool prefix",
                        )
                    })?;
                    let completed_operation =
                        continuation.operations.get(index).copied().ok_or_else(|| {
                            AgentError::new(
                                AgentErrorKind::Internal,
                                "effect-decision barrier lost a completed tool operation",
                            )
                        })?;
                    Ok(ContinuationToolSlot::Completed {
                        call: slot_call,
                        result_entry_id,
                        item,
                        operation_id: completed_operation,
                    })
                }
                std::cmp::Ordering::Equal => Ok(ContinuationToolSlot::PendingDecision {
                    call: slot_call,
                    result_entry_id,
                    binding_digest,
                }),
                std::cmp::Ordering::Greater => Ok(ContinuationToolSlot::NotAdmitted {
                    call: slot_call,
                    result_entry_id,
                }),
            },
        )
        .collect::<Result<Vec<_>, AgentError>>()?;
    if continuation.operations.len() != continuation.source_index {
        return Err(AgentError::new(
            AgentErrorKind::Internal,
            "effect-decision barrier contains mixed batch progress",
        ));
    }
    Ok(slots)
}
