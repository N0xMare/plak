//! Model request preparation, final admission and controlled polling.

use crate::runtime::admission::validate_configuration_runtime_authority;
use crate::runtime::compaction::request::compact_runtime_endpoint_error;
use crate::runtime::continuation::{previous_response, runtime_component, runtime_manifest_error};
use crate::runtime::driver::model_cleanup::{
    ModelCleanupOutcome, agent_model_timeout, ambiguous_compaction_error,
    join_cancelled_control_start, join_cancelled_control_stream,
};
use crate::runtime::driver::{Driver, ModelOperationResult, ModelStartAdmission};
use crate::runtime::observation::budget::observation_bytes_for_sent_status;
use crate::runtime::observation::model_event_stamps_ttft;
use crate::runtime::request_slots::RequestSlot;
use crate::runtime::terminal::{agent_terminal_status, cancelled_error};
use crate::runtime::{OperationId, PromptCommand};
use crate::{
    AgentError, AgentErrorKind, AgentEventKind, AgentSessionEndpointError, DurableOperationKind,
    DurableOperationState, EventContext, FamilyArtifact, RuntimeComponentKind, StoreAdmission,
    TelemetryObservationKind,
};
use futures_util::StreamExt;
use plakous_model::{
    ActiveFamilyArtifact, AttemptId, ContextStatus, ContextStatusKind, ConversationView,
    ModelCacheAffinity, ModelCallContext, ModelError, ModelErrorKind, ModelErrorPhase, ModelEvent,
    ModelOperationRoot, ModelPreparationContext, ModelPreparationError, ModelRequest,
    ModelResponse, ModelStreamItem, ProviderCallRecoveryLocatorStage, RequestMetadata,
    StableRequestPrefix, ToolChoice, Usage, ValidatedRequestPlan, ValidationMode,
};
use std::num::NonZeroU32;
use std::sync::Arc;
impl Driver {
    fn request_conversation(&self) -> ConversationView {
        self.conversation()
    }

    pub(super) fn with_completed_history_prefix(
        &self,
        plan: &ValidatedRequestPlan,
        mut context: ModelPreparationContext,
    ) -> ModelPreparationContext {
        let checkpoint = self.authority.checkpoint();
        let manifest = checkpoint.manifest();
        if let Some(prefix) = completed_history_prefix(
            manifest.canonical().history(),
            manifest.lifecycle(),
            plan.request().input(),
        ) {
            context = context.with_completed_history_prefix(prefix);
        }
        context
    }

    pub(super) fn with_previous_response_metadata(
        &self,
        plan: &ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<ModelPreparationContext, AgentError> {
        let context = self
            .with_completed_history_prefix(plan, context)
            .with_request_anchor(self.request_anchor.clone());
        let checkpoint = self.authority.checkpoint();
        let manifest = checkpoint.manifest();
        let context = context.with_turn_position(super::turn_position::turn_position(
            manifest.canonical().history(),
        ));
        let Some((metadata, response_id)) =
            previous_response(manifest.ledger(), plan, context.authority())?
        else {
            return Ok(context);
        };
        Ok(context
            .with_previous_response_metadata(metadata)
            .with_previous_response_id(response_id))
    }

    pub(super) fn active_model_preparation_state(
        &self,
    ) -> Result<(Option<ActiveFamilyArtifact>, ModelCacheAffinity), AgentError> {
        let authority = self.authority.checkpoint();
        let manifest = authority.manifest();
        let artifact = manifest
            .family_artifact()
            .map(FamilyArtifact::to_active)
            .transpose()
            .map_err(runtime_manifest_error)?;
        Ok((artifact, manifest.canonical().context().affinity().clone()))
    }

    /// Names the stable-prefix fields that differ between `request` and the
    /// round that produced `active`, when this driver saw that round.
    ///
    /// This only makes the drift observable: a Responses route keeps
    /// replaying the artifact when only `tool-choice` differs and drops it to
    /// prepare fresh for any other field. A producer from before this driver
    /// started (a reopened session) is unknown and reports nothing.
    fn artifact_prefix_drift(
        &self,
        active: Option<&ActiveFamilyArtifact>,
        request: &ModelRequest,
    ) -> Option<plakous_model::StableRequestPrefixDrift> {
        let (binding, producer) = self.artifact_producer_prefix.as_ref()?;
        if active?.binding() != *binding {
            return None;
        }
        let drift = request.stable_prefix().drift_from(producer);
        (!drift.is_empty()).then_some(drift)
    }

    pub(super) fn validate_provider_artifact_candidate(
        &self,
        plan: &ValidatedRequestPlan,
        response: &ModelResponse,
    ) -> Result<(), AgentError> {
        let Some(candidate) = response.family_artifact_candidate() else {
            return Ok(());
        };
        if self
            .provider_configuration
            .effective()
            .descriptor()
            .integration_class()
            == plakous_model::IntegrationClass::Custom
        {
            return Err(AgentError::new(
                AgentErrorKind::Model,
                "custom model integration cannot produce a managed family artifact",
            ));
        }
        let cache_affinity = self
            .authority
            .checkpoint()
            .manifest()
            .canonical()
            .context()
            .affinity()
            .identity();
        if candidate.producer().cache_affinity() != Some(cache_affinity) {
            return Err(AgentError::new(
                AgentErrorKind::Model,
                "family artifact candidate belongs to a foreign cache affinity",
            ));
        }
        if self.request_conversation().identity() != plan.request().input().identity() {
            return Err(AgentError::new(
                AgentErrorKind::Model,
                "family artifact candidate belongs to a stale canonical projection",
            ));
        }
        let covered_projection = plan
            .request()
            .input()
            .staged_canonical_item_prefix_identity(response.output())
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Model,
                    "family artifact staged canonical item coverage is invalid",
                )
            })?;
        candidate
            .validate_for_commit(plan, covered_projection, response.output().len())
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Model,
                    "family artifact does not match the provider response",
                )
            })?;
        FamilyArtifact::from_candidate(candidate)
            .map(|_| ())
            .map_err(runtime_manifest_error)
    }

    /// Prepares one turn round, optionally with tool selection withdrawn.
    ///
    /// `tools_disabled` is the operation-budget wrap-up round: the exact live
    /// prefix, tools and all, with [`ToolChoice::None`] so the model can only
    /// answer. Every wire family expresses that natively (`tool_choice: "none"`
    /// on Messages, the Responses dialect and Chat Completions), and a request
    /// that cannot carry it fails validation rather than silently keeping tools.
    pub(super) fn model_request(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        tools_disabled: bool,
    ) -> Result<ModelRequest, AgentError> {
        let prefix = if tools_disabled {
            tool_free_prefix(&self.stable_request_prefix)?
        } else {
            self.stable_request_prefix.clone()
        };
        ModelRequest::from_parts(
            prefix,
            self.request_conversation(),
            RequestMetadata::for_turn(self.run_id, command.turn_id, attempt_id, self.clock.now()),
        )
        .map_err(|error| {
            self.project_model_error(
                ModelError::new(
                    ModelErrorKind::InvalidRequest,
                    format!("canonical model request is invalid: {error}"),
                ),
                ModelErrorPhase::Preparation,
            )
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the model operation keeps cancellation, control, assembly, and cleanup ordering visible in one auditable state machine"
    )]
    pub(super) async fn run_model(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        request: ModelRequest,
    ) -> ModelOperationResult {
        let _slot = match self.take_request_slot(command).await {
            Ok(slot) => slot,
            Err(error) => {
                return ModelOperationResult {
                    result: Err(error),
                    usage: None,
                    operation_id: None,
                    plan: None,
                    request_estimate_tokens: None,
                    request_wire_input: None,
                    request_context_status: None,
                    wire_output_limit: None,
                };
            }
        };
        let started = tokio::time::Instant::now();
        let operation_deadline = started + self.config.model_operation_timeout;
        let context = self
            .telemetry_context
            .with_turn(command.turn_id)
            .with_attempt(attempt_id);
        self.emit_telemetry(context, TelemetryObservationKind::ModelStarted);
        let mut usage = None;
        let mut output_deltas = 0;
        let mut operation_id = None;
        let mut validated_plan = None;
        let mut request_estimate_tokens = None;
        let mut request_wire_input = None;
        let mut request_context_status = None;
        let mut prepared_wire_output_limit = None;
        let mut result = Box::pin(self.run_model_operation(
            command,
            attempt_id,
            request,
            started,
            operation_deadline,
            &mut usage,
            &mut operation_id,
            &mut validated_plan,
            &mut request_estimate_tokens,
            &mut request_wire_input,
            &mut request_context_status,
            &mut prepared_wire_output_limit,
            &mut output_deltas,
        ))
        .await
        .map(|response| self.land_tool_calls(command, attempt_id, response));
        if let Err(error) = &mut result {
            error.fill_missing_wire_output_limit(prepared_wire_output_limit);
        }
        if let Ok(response) = &result {
            usage = response.usage().or(usage);
        }
        let model_elapsed = started.elapsed();
        let served_by = result
            .as_ref()
            .ok()
            .and_then(|response| response.metadata().served_by());
        self.emit_telemetry(
            context,
            TelemetryObservationKind::ModelFinished {
                elapsed: model_elapsed,
                status: agent_terminal_status(&result, self.shutdown.is_cancelled()),
                usage,
                served_by,
                output_deltas,
            },
        );
        command.shared.record_model_telemetry(model_elapsed);
        ModelOperationResult {
            result,
            usage,
            operation_id,
            plan: validated_plan,
            request_estimate_tokens,
            request_wire_input,
            request_context_status,
            wire_output_limit: prepared_wire_output_limit,
        }
    }

    /// Takes a slot in the host's shared request budget, if it set one,
    /// before any deadline of this request starts. A full budget publishes
    /// one `ModelRequestQueued` and waits in arrival order; cancellation,
    /// shutdown and a lost lease end the wait without taking a slot.
    pub(super) async fn take_request_slot(
        &mut self,
        command: &PromptCommand,
    ) -> Result<Option<RequestSlot>, AgentError> {
        let Some(slots) = self.request_slots.clone() else {
            return Ok(None);
        };
        if let Some(slot) = slots.try_take() {
            return Ok(Some(slot));
        }
        let _ = command.shared.events.publish(
            EventContext::turn(command.turn_id),
            AgentEventKind::ModelRequestQueued {
                slots: slots.label().to_owned(),
            },
        );
        let take = slots.take();
        tokio::pin!(take);
        loop {
            tokio::select! {
                slot = &mut take => return Ok(Some(slot)),
                () = self.authority.lease_lost() => return Err(self.authority.heartbeat_failure()),
                () = command.cancellation.cancelled() => return Err(cancelled_error()),
                () = self.shutdown.cancelled() => return Err(cancelled_error()),
                control = self.control_receiver.recv() => {
                    let Some(control) = control else {
                        return Err(cancelled_error());
                    };
                    Box::pin(self.handle_control(Some(command), control)).await;
                }
            }
        }
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "the model operation keeps cancellation, control, assembly, and cleanup ordering visible in one auditable state machine"
    )]
    async fn run_model_operation(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        request: ModelRequest,
        started: tokio::time::Instant,
        operation_deadline: tokio::time::Instant,
        observed_usage: &mut Option<Usage>,
        operation_id: &mut Option<OperationId>,
        validated_plan: &mut Option<ValidatedRequestPlan>,
        request_estimate_tokens: &mut Option<u64>,
        request_wire_input: &mut Option<plakous_model::ContextWireInput>,
        request_context_status: &mut Option<Box<ContextStatus>>,
        prepared_wire_output_limit: &mut Option<NonZeroU32>,
        output_deltas: &mut u64,
    ) -> Result<ModelResponse, AgentError> {
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
        *validated_plan = Some(plan.clone());
        let _ = command.shared.events.publish(
            EventContext::attempt(command.turn_id, attempt_id),
            AgentEventKind::ModelStarted,
        );
        let prepared_at = self.clock.now();
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
        let (active_family_artifact, cache_affinity) = self.active_model_preparation_state()?;
        if let Some(fields) =
            self.artifact_prefix_drift(active_family_artifact.as_ref(), plan.request())
        {
            self.emit_telemetry(
                self.telemetry_context
                    .with_turn(command.turn_id)
                    .with_attempt(attempt_id),
                TelemetryObservationKind::ModelArtifactPrefixDrift { fields },
            );
        }
        let mut preparation_context = ModelPreparationContext::new(authority, prepared_at)
            .with_cache_affinity(cache_affinity);
        preparation_context = self.with_previous_response_metadata(&plan, preparation_context)?;
        if let Some(artifact) = active_family_artifact {
            preparation_context = preparation_context.with_active_family_artifact(artifact);
        }
        let prepared = Arc::clone(&self.model)
            .prepare_call(plan.clone(), preparation_context)
            .map_err(ModelError::from)
            .map_err(|error| self.project_model_error(error, ModelErrorPhase::Preparation))?;
        *prepared_wire_output_limit = prepared
            .record()
            .projection()
            .output_limit()
            .map(plakous_model::PreparedOutputLimit::tokens);
        let sent_status = descriptor
            .context_status(&plan, prepared.record().projection(), prepared_at)
            .map(|status| {
                status.with_input_parts(
                    prepared
                        .record()
                        .projection()
                        .token_projection()
                        .input_parts(),
                )
            });
        self.request_anchor = prepared
            .record()
            .projection()
            .token_projection()
            .request_anchor()
            .cloned()
            .or_else(|| self.request_anchor.clone());
        *request_wire_input = prepared
            .record()
            .projection()
            .token_projection()
            .wire_input()
            .cloned();
        *request_estimate_tokens = sent_status
            .as_ref()
            .ok()
            .and_then(ContextStatus::occupancy_estimate_tokens);
        *request_context_status = sent_status.as_ref().ok().cloned().map(Box::new);
        let _ = command.shared.events.publish(
            EventContext::attempt(command.turn_id, attempt_id),
            AgentEventKind::RequestPrepared {
                input_parts: sent_status
                    .as_ref()
                    .ok()
                    .and_then(ContextStatus::input_parts),
                occupied_tokens: sent_status
                    .as_ref()
                    .ok()
                    .and_then(ContextStatus::occupancy_tokens),
                prepared_input_upper_bound: sent_status.as_ref().ok().and_then(|status| {
                    status.local_input_upper_bound().or_else(|| {
                        status
                            .measurement()
                            .map(|measurement| measurement.bound().upper_bound())
                    })
                }),
                tool_names: plan
                    .request()
                    .tools()
                    .iter()
                    .map(|tool| tool.definition().name().clone())
                    .collect(),
            },
        );
        self.observation_budget = match sent_status {
            Ok(status) => match status.kind() {
                ContextStatusKind::Fits { headroom } => {
                    crate::runtime::observation::budget::ObservationBudget::new(
                        observation_bytes_for_sent_status(
                            *headroom,
                            status.uses_unanchored_estimate(),
                        ),
                        self.config.observation_budget_cap_bytes,
                    )
                }
                _ => crate::runtime::observation::budget::ObservationBudget::new(0, None),
            },
            Err(_) => crate::runtime::observation::budget::ObservationBudget::new(0, None),
        };
        let prepared_record = prepared.record().clone();
        let prepared_record_bytes = serde_json::to_vec(&prepared_record).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "prepared provider call record could not be captured",
            )
        })?;
        let prepared_operation = self
            .prepare_operation(
                StoreAdmission::Ordinary,
                command.turn_id,
                DurableOperationKind::Provider {
                    root: ModelOperationRoot::Turn(command.turn_id),
                    attempt_id,
                },
                prepared_record.identity(),
                Some(prepared_record_bytes),
                None,
            )
            .await?;
        *operation_id = Some(prepared_operation);
        if let Some(locator) = prepared_record.recovery_locator() {
            let evidence = runtime_component(
                RuntimeComponentKind::Operation,
                serde_json::to_vec(locator).map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "prepared provider recovery locator could not be encoded",
                    )
                })?,
            )?;
            self.transition_operation(
                command.turn_id,
                prepared_operation,
                DurableOperationState::LocatorBound(evidence),
            )
            .await?;
        }
        let start = {
            if tokio::time::Instant::now() >= operation_deadline {
                return Err(self.model_timeout_error());
            }
            let provider_dispatch_gate = Arc::clone(&self.provider_dispatch_gate);
            let _dispatch_guard = provider_dispatch_gate.lock().await;
            if command.cancellation.is_cancelled() || self.shutdown.is_cancelled() {
                return Err(cancelled_error());
            }
            if tokio::time::Instant::now() >= operation_deadline {
                return Err(self.model_timeout_error());
            }
            let dispatch_checked_at = self.clock.now();
            let dispatch_authority = Arc::clone(&self.model)
                .revalidate_runtime_authority(dispatch_checked_at)
                .await
                .map_err(ModelPreparationError::from)
                .map_err(ModelError::from)
                .map_err(|error| self.project_model_error(error, ModelErrorPhase::Preparation))?;
            validate_configuration_runtime_authority(
                &self.provider_configuration,
                &descriptor,
                &dispatch_authority,
                dispatch_checked_at,
            )
            .map_err(ModelError::from)
            .map_err(|error| self.project_model_error(error, ModelErrorPhase::Preparation))?;
            prepared_record
                .revalidate(&descriptor, &plan, dispatch_checked_at)
                .and_then(|()| {
                    prepared_record.revalidate_selected_authority(
                        &dispatch_authority,
                        &descriptor,
                        dispatch_checked_at,
                    )
                })
                .map_err(ModelError::from)
                .map_err(|error| self.project_model_error(error, ModelErrorPhase::Preparation))?;
            self.authority.check_heartbeat()?;
            command.shared.begin_model_streaming()?;
            self.transition_operation(
                command.turn_id,
                prepared_operation,
                DurableOperationState::Dispatched,
            )
            .await?;
            let context = ModelCallContext::new(attempt_id, command.cancellation.clone(), None)
                .with_dispatch_timeout(self.config.model_operation_timeout);
            let _lease_guard = self.start_authority().admit().await?;
            if command.cancellation.is_cancelled() || self.shutdown.is_cancelled() {
                return Err(cancelled_error());
            }
            if tokio::time::Instant::now() >= operation_deadline {
                return Err(self.model_timeout_error());
            }
            let mut start = prepared.start_with_control(context);
            let mut task_context = std::task::Context::from_waker(std::task::Waker::noop());
            let _ = command.shared.events.publish(
                EventContext::attempt(command.turn_id, attempt_id),
                AgentEventKind::ModelDispatchStarted,
            );
            match start.as_mut().poll(&mut task_context) {
                std::task::Poll::Pending => ModelStartAdmission::Pending(start),
                std::task::Poll::Ready(result) => ModelStartAdmission::Ready(result),
            }
        };
        let digest = plan.digest();
        let operation_timeout = tokio::time::sleep_until(operation_deadline);
        tokio::pin!(operation_timeout);
        let mut stream = match start {
            ModelStartAdmission::Ready(result) => {
                if command.cancellation.is_cancelled() || self.shutdown.is_cancelled() {
                    command.cancellation.cancel();
                    let acknowledgement = match result {
                        Ok(mut stream) => {
                            join_cancelled_control_stream(
                                &mut stream,
                                self.config.model_cleanup_timeout,
                            )
                            .await
                        }
                        Err(error) => ModelCleanupOutcome::Joined(Some(error)),
                    };
                    return Err(self.project_cancelled_model_error(acknowledgement));
                }
                result
                    .map_err(|error| self.project_model_error(error, ModelErrorPhase::Operation))?
            }
            ModelStartAdmission::Pending(mut start) => loop {
                tokio::select! {
                    () = self.authority.lease_lost() => {
                        command.cancellation.cancel();
                        let _ = join_cancelled_control_start(start, self.config.model_cleanup_timeout).await;
                        return Err(self.authority.heartbeat_failure());
                    }
                    () = &mut operation_timeout => {
                        command.cancellation.cancel();
                        return Err(self.project_timed_out_model_error(join_cancelled_control_start(start, self.config.model_cleanup_timeout).await));
                    }
                    () = command.cancellation.cancelled() => {
                        return Err(self.project_cancelled_model_error(join_cancelled_control_start(start, self.config.model_cleanup_timeout).await));
                    }
                    () = self.shutdown.cancelled() => {
                        command.cancellation.cancel();
                        return Err(self.project_cancelled_model_error(join_cancelled_control_start(start, self.config.model_cleanup_timeout).await));
                    }
                    control = self.control_receiver.recv() => {
                        let Some(control) = control else {
                            command.cancellation.cancel();
                            return Err(self.project_cancelled_model_error(join_cancelled_control_start(start, self.config.model_cleanup_timeout).await));
                        };
                        Box::pin(self.handle_control(Some(command), control)).await;
                    }
                    result = &mut start => {
                        if command.cancellation.is_cancelled() || self.shutdown.is_cancelled() {
                            command.cancellation.cancel();
                            let acknowledgement = match result {
                                Ok(mut stream) => join_cancelled_control_stream(&mut stream, self.config.model_cleanup_timeout).await,
                                Err(error) => ModelCleanupOutcome::Joined(Some(error)),
                            };
                            return Err(self.project_cancelled_model_error(acknowledgement));
                        }
                        break result.map_err(|error| {
                            self.project_model_error(error, ModelErrorPhase::Operation)
                        })?;
                    }
                }
            },
        };
        let mut completed = None;
        let mut ttft_stamped = false;
        let mut learned_locator = false;
        loop {
            let next = tokio::select! {
                () = self.authority.lease_lost() => {
                    command.cancellation.cancel();
                    let _ = join_cancelled_control_stream(&mut stream, self.config.model_cleanup_timeout).await;
                    return Err(self.authority.heartbeat_failure());
                }
                () = command.cancellation.cancelled() => {
                    return Err(self.project_cancelled_model_error(join_cancelled_control_stream(&mut stream, self.config.model_cleanup_timeout).await));
                }
                () = self.shutdown.cancelled() => {
                    command.cancellation.cancel();
                    return Err(self.project_cancelled_model_error(join_cancelled_control_stream(&mut stream, self.config.model_cleanup_timeout).await));
                }
                control = self.control_receiver.recv() => {
                    let Some(control) = control else {
                        command.cancellation.cancel();
                        return Err(self.project_cancelled_model_error(join_cancelled_control_stream(&mut stream, self.config.model_cleanup_timeout).await));
                    };
                    Box::pin(self.handle_control(Some(command), control)).await;
                    continue;
                }
                event = stream.next() => event,
            };
            if command.cancellation.is_cancelled() || self.shutdown.is_cancelled() {
                command.cancellation.cancel();
                let acknowledgement = match next {
                    Some(Err(error)) => ModelCleanupOutcome::Joined(Some(error)),
                    Some(Ok(_)) => {
                        join_cancelled_control_stream(
                            &mut stream,
                            self.config.model_cleanup_timeout,
                        )
                        .await
                    }
                    None => ModelCleanupOutcome::Joined(None),
                };
                return Err(self.project_cancelled_model_error(acknowledgement));
            }
            let Some(item) = next else {
                break;
            };
            let item =
                item.map_err(|error| self.project_model_error(error, ModelErrorPhase::Stream))?;
            let event = match item {
                ModelStreamItem::RecoveryLocatorBound(locator) => {
                    if learned_locator
                        || locator.availability_stage()
                            != ProviderCallRecoveryLocatorStage::LearnedAfterDispatch
                        || prepared_record
                            .recovery_request_with_locator(locator.clone())
                            .is_err()
                    {
                        command.cancellation.cancel();
                        let _ = join_cancelled_control_stream(
                            &mut stream,
                            self.config.model_cleanup_timeout,
                        )
                        .await;
                        return Err(ambiguous_compaction_error(
                            "provider produced contradictory recovery locator evidence",
                        ));
                    }
                    let evidence = runtime_component(
                        RuntimeComponentKind::Operation,
                        serde_json::to_vec(&locator).map_err(|_| {
                            AgentError::new(
                                AgentErrorKind::Internal,
                                "learned provider recovery locator could not be encoded",
                            )
                        })?,
                    )?;
                    if self
                        .transition_operation(
                            command.turn_id,
                            prepared_operation,
                            DurableOperationState::LocatorBound(evidence),
                        )
                        .await
                        .is_err()
                    {
                        command.cancellation.cancel();
                        let _ = join_cancelled_control_stream(
                            &mut stream,
                            self.config.model_cleanup_timeout,
                        )
                        .await;
                        return Err(ambiguous_compaction_error(
                            "learned provider recovery locator was not durably bound",
                        ));
                    }
                    learned_locator = true;
                    continue;
                }
                ModelStreamItem::Event(event) => event,
            };
            if completed.is_some() {
                return Err(self.model_failure(
                    ModelErrorKind::InvalidEvent,
                    "model operation produced an event after its terminal response",
                    ModelErrorPhase::Terminal,
                ));
            }
            if !ttft_stamped && model_event_stamps_ttft(&event) {
                ttft_stamped = true;
                self.emit_telemetry(
                    self.telemetry_context
                        .with_turn(command.turn_id)
                        .with_attempt(attempt_id),
                    TelemetryObservationKind::ModelFirstEvent {
                        elapsed: started.elapsed(),
                    },
                );
            }
            if let ModelEvent::StreamActivity(activity) = &event {
                self.emit_telemetry(
                    self.telemetry_context
                        .with_turn(command.turn_id)
                        .with_attempt(attempt_id),
                    TelemetryObservationKind::ModelStreamActivity {
                        activity: *activity,
                    },
                );
                continue;
            }
            if let Some(kind) = prompt_cache_diagnosis_kind(&event) {
                self.emit_telemetry(
                    self.telemetry_context
                        .with_turn(command.turn_id)
                        .with_attempt(attempt_id),
                    kind,
                );
                continue;
            }
            if matches!(
                &event,
                ModelEvent::TextDelta(_)
                    | ModelEvent::ReasoningDelta(_)
                    | ModelEvent::ToolInputDelta(_)
            ) {
                *output_deltas = output_deltas.saturating_add(1);
            }
            if let ModelEvent::Usage(update) = &event {
                *observed_usage = Some(update.usage());
            }
            if let ModelEvent::Retry(retry) = &event {
                self.telemetry.observe_model_retry(
                    self.telemetry_context
                        .with_turn(command.turn_id)
                        .with_attempt(attempt_id),
                    retry,
                );
            }
            // A replay reports its own usage; the discarded request's partial
            // count must not survive into the turn's total.
            if let ModelEvent::Retry(retry) = &event
                && retry.output_discarded()
            {
                *observed_usage = None;
                *output_deltas = 0;
            }
            if let ModelEvent::ResponseCompleted(response) = &event {
                if response.plan_digest() != digest {
                    return Err(self.model_failure(
                        ModelErrorKind::InvalidEvent,
                        "terminal model response belongs to another prepared plan",
                        ModelErrorPhase::Terminal,
                    ));
                }
                response
                    .validate_for_commit()
                    .map_err(|error| self.project_model_error(error, ModelErrorPhase::Terminal))?;
                *observed_usage = response.usage().or(*observed_usage);
                completed = Some(response.clone());
            }
            let _ = command.shared.events.publish(
                EventContext::attempt(command.turn_id, attempt_id),
                AgentEventKind::model(event),
            );
        }
        completed.ok_or_else(|| {
            self.model_failure(
                ModelErrorKind::IncompleteResponse,
                "model stream ended without a terminal response",
                ModelErrorPhase::Terminal,
            )
        })
    }

    pub(super) fn project_model_error(
        &self,
        mut error: ModelError,
        phase: ModelErrorPhase,
    ) -> AgentError {
        error.ensure_context(&self.model_error_target, phase);
        let error = AgentError::from_model(error);
        if self.compaction_in_flight {
            error
        } else {
            self.with_last_compaction_diagnostic(error)
        }
    }

    pub(super) fn with_last_compaction_diagnostic(&self, error: AgentError) -> AgentError {
        if error.model_kind() == Some(ModelErrorKind::ContextLimit)
            && let Some(diagnostic) = &self.last_compaction_diagnostic
            && !error.message().ends_with(diagnostic)
        {
            error.with_runtime_diagnostic(diagnostic)
        } else {
            error
        }
    }

    pub(super) fn compact_preparation_endpoint_error(
        &self,
        error: ModelError,
    ) -> AgentSessionEndpointError {
        compact_runtime_endpoint_error(
            &self.project_model_error(error, ModelErrorPhase::Preparation),
        )
    }

    fn project_cancelled_model_error(&self, outcome: ModelCleanupOutcome) -> AgentError {
        match outcome {
            ModelCleanupOutcome::Joined(error) => {
                let mut error = error.unwrap_or_else(ModelError::cancelled);
                error.ensure_context(&self.model_error_target, ModelErrorPhase::Cleanup);
                AgentError::from_cancelled_model(error)
            }
            ModelCleanupOutcome::Unjoined(mut error) => {
                error.ensure_context(&self.model_error_target, ModelErrorPhase::Cleanup);
                AgentError::from_outcome_unknown_model(error)
            }
        }
    }

    fn project_timed_out_model_error(&self, outcome: ModelCleanupOutcome) -> AgentError {
        match outcome {
            ModelCleanupOutcome::Unjoined(mut error) => {
                error.ensure_context(&self.model_error_target, ModelErrorPhase::Cleanup);
                AgentError::from_outcome_unknown_model(error)
            }
            ModelCleanupOutcome::Joined(Some(error))
                if !matches!(
                    error.kind(),
                    ModelErrorKind::Cancelled | ModelErrorKind::Timeout
                ) =>
            {
                self.project_model_error(error, ModelErrorPhase::Cleanup)
            }
            ModelCleanupOutcome::Joined(_) => self.model_timeout_error(),
        }
    }

    pub(super) fn model_timeout_error(&self) -> AgentError {
        agent_model_timeout(
            &self.model_error_target,
            "model operation exceeded its configured deadline",
        )
    }

    pub(super) fn model_failure(
        &self,
        kind: ModelErrorKind,
        message: &'static str,
        phase: ModelErrorPhase,
    ) -> AgentError {
        self.project_model_error(ModelError::new(kind, message), phase)
    }
}

/// The live prefix with tool selection withdrawn and nothing else changed.
///
/// Tools stay declared so the prompt prefix, and its cache affinity, are the
/// ones the turn has been using; only the selection policy changes.
fn tool_free_prefix(source: &StableRequestPrefix) -> Result<StableRequestPrefix, AgentError> {
    if source.tool_choice() == &ToolChoice::None {
        return Ok(source.clone());
    }
    let mut prefix = StableRequestPrefix::new(
        source.selected_provider_configuration_id(),
        source.effective_provider_configuration_id(),
        source.processing_tier().clone(),
        source.instructions().to_vec(),
        source.tools().to_vec(),
        source.hosted_tools().to_vec(),
        source.output().clone(),
        source.reasoning(),
        ToolChoice::None,
        source.cache(),
        source.limits().clone(),
        source.native().clone(),
        source.resolved_harness_plan_identity(),
    )
    .and_then(|prefix| {
        prefix.with_context_admission_requirement(source.context_admission_requirement())
    })
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Internal,
            "operation-budget wrap-up request prefix is invalid",
        )
    })?;
    if let Some(projection) = source.tool_projection() {
        prefix = prefix
            .with_tool_projection(projection.clone())
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "operation-budget wrap-up tool projection is invalid",
                )
            })?;
    }
    Ok(prefix)
}

/// Maps a cache-miss diagnosis to telemetry. Other events are not observations.
fn prompt_cache_diagnosis_kind(event: &ModelEvent) -> Option<TelemetryObservationKind> {
    let ModelEvent::PromptCacheDiagnosis(diagnosis) = event else {
        return None;
    };
    Some(TelemetryObservationKind::PromptCacheDiagnosis {
        reason: diagnosis.reason,
        missed_input_tokens: diagnosis.missed_input_tokens,
    })
}

/// Root-intent segments and lifecycle authority identify closed history even
/// when the final item is a cancelled tool result or a steering user message.
pub(in crate::runtime) fn completed_history_prefix(
    history: &plakous_model::CanonicalHistory,
    lifecycle: &crate::RuntimeLifecycle,
    input: &ConversationView,
) -> Option<plakous_model::ConversationPrefixIdentity> {
    let segments = history.segments();
    let closed_segments = match lifecycle {
        crate::RuntimeLifecycle::Quiescent
        | crate::RuntimeLifecycle::Working {
            root: crate::RuntimeWorkingRoot::SessionWorkflow(_),
            ..
        } => segments.len(),
        crate::RuntimeLifecycle::Working { .. } => segments
            .iter()
            .enumerate()
            .filter_map(|(index, segment)| {
                (segment.kind() == plakous_model::HistorySegmentKind::RecentIntent).then_some(index)
            })
            .last()
            .unwrap_or(0),
    };
    // Context planning can elide bodies and select older groups. Derive the
    // boundary from their retained entry identities, then bind the proof
    // to the exact final request projection, including those transformations.
    let closed_entries = segments
        .iter()
        .take(closed_segments)
        .flat_map(|segment| {
            segment
                .items()
                .iter()
                .map(plakous_model::ConversationItem::id)
        })
        .collect::<std::collections::BTreeSet<_>>();
    let closed_items = input
        .items()
        .iter()
        .take_while(|item| {
            closed_entries.contains(&item.id())
                || item
                    .blocks()
                    .iter()
                    .all(|block| matches!(block, plakous_model::ContentBlock::Compaction(_)))
        })
        .count();
    (closed_items > 0)
        .then(|| input.semantic_prefix_identity(closed_items).ok())
        .flatten()
}

#[cfg(test)]
mod prompt_cache_diagnosis_tests {
    use super::prompt_cache_diagnosis_kind;
    use crate::TelemetryObservationKind;
    use plakous_model::{ModelEvent, PromptCacheDiagnosis, PromptCacheMissReason};

    #[test]
    fn diagnosis_is_content_free_telemetry() {
        let event = ModelEvent::PromptCacheDiagnosis(PromptCacheDiagnosis {
            reason: PromptCacheMissReason::Other,
            missed_input_tokens: Some(587),
        });
        assert!(matches!(
            prompt_cache_diagnosis_kind(&event),
            Some(TelemetryObservationKind::PromptCacheDiagnosis {
                reason: PromptCacheMissReason::Other,
                missed_input_tokens: Some(587),
            })
        ));
        let omitted = ModelEvent::PromptCacheDiagnosis(PromptCacheDiagnosis {
            reason: PromptCacheMissReason::MessagesChanged,
            missed_input_tokens: None,
        });
        assert!(matches!(
            prompt_cache_diagnosis_kind(&omitted),
            Some(TelemetryObservationKind::PromptCacheDiagnosis {
                reason: PromptCacheMissReason::MessagesChanged,
                missed_input_tokens: None,
            })
        ));
        let rendered = format!("{event:?}");
        assert!(!rendered.contains("msg_"));
        assert!(
            prompt_cache_diagnosis_kind(&ModelEvent::StreamActivity(
                plakous_model::ModelStreamActivity::default()
            ))
            .is_none()
        );
    }
}

#[cfg(test)]
mod completed_history_tests {
    use super::completed_history_prefix;
    use crate::{RuntimeLifecycle, RuntimeWorkingRoot, WorkingRecoveryPhase};
    use plakous_model::{
        Actor, CanonicalHistory, ContentBlock, ConversationItem, ConversationView, EntryId,
        HistorySegment, HistorySegmentKind, Provenance, TextBlock, TurnId, WorkingProjection,
    };

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn item(
        id: u128,
        actor: Actor,
        text: &str,
    ) -> Result<ConversationItem, Box<dyn std::error::Error>> {
        Ok(ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(id))?,
            actor,
            vec![ContentBlock::Text(TextBlock::new(text, None)?)],
            if actor == Actor::User {
                Provenance::UserInput
            } else {
                Provenance::ModelOutput
            },
        )?)
    }

    #[test]
    fn steering_and_assistant_continuations_do_not_close_the_active_root() -> TestResult {
        let history = CanonicalHistory::new(vec![
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![item(1, Actor::User, "old")?],
            )?,
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![item(2, Actor::Assistant, "done")?],
            )?,
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![item(3, Actor::User, "active")?],
            )?,
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![item(4, Actor::Assistant, "continuing")?],
            )?,
            HistorySegment::new(
                HistorySegmentKind::Steering,
                vec![item(5, Actor::User, "steer")?],
            )?,
        ])?;
        let input = ConversationView::from_shared(history.clone(), WorkingProjection::default());
        let working = RuntimeLifecycle::Working {
            root: RuntimeWorkingRoot::Turn {
                turn_id: TurnId::try_from(uuid::Uuid::from_u128(6))?,
                next_round: 2,
            },
            recovery_phase: WorkingRecoveryPhase::DispatchEvidence,
            obligations: Vec::new(),
        };
        assert_eq!(
            completed_history_prefix(&history, &working, &input),
            Some(input.semantic_prefix_identity(2)?),
        );
        // The same tail is closed only after terminal checkpoint publication.
        assert_eq!(
            completed_history_prefix(&history, &RuntimeLifecycle::Quiescent, &input),
            Some(input.semantic_prefix_identity(5)?),
        );
        // Elision changes content while retaining its entry identity. Bind the
        // proof to the exact selected projection, not the old canonical bytes.
        let selected = ConversationView::from_items(vec![
            item(2, Actor::Assistant, "elided older body")?,
            item(4, Actor::Assistant, "continuing")?,
            item(5, Actor::User, "steer")?,
        ])?;
        assert_eq!(
            completed_history_prefix(&history, &working, &selected),
            Some(selected.semantic_prefix_identity(1)?),
        );
        Ok(())
    }
}
