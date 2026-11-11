//! Dispatching a prepared compaction and consuming its stream.

use super::{
    CompactionAttempt, CompactionDetail, MeasuredSource, PreparedCompaction,
    compaction_provider_error,
};
use crate::runtime::OperationId;
use crate::runtime::admission::validate_configuration_runtime_authority;
use crate::runtime::compaction::live::{CompactionStreamError, LiveCompaction};
use crate::runtime::compaction::request::{
    compact_runtime_endpoint_error, compact_runtime_endpoint_rejected,
};
use crate::runtime::compaction::{
    DurableCompactionIdentity, DurableCompactionRequestRecord, ExactCompactionRuntime,
};
use crate::runtime::continuation::runtime_manifest_error;
use crate::runtime::driver::commands::ordinary_capacity_refusal;
use crate::runtime::driver::model_cleanup::{
    abandoned_compaction_error, ambiguous_compaction_error, join_cancelled_control_start,
    join_cancelled_control_stream,
};
use crate::runtime::driver::operations::transition_admission;
use crate::runtime::driver::{Driver, ModelStartAdmission};
use crate::runtime::operation_record::working_lifecycle_for_root;
use crate::runtime::request_slots::RequestSlot;
use crate::runtime::terminal::cancelled_error;
use crate::{
    AgentCompactSessionOutcome, AgentError, AgentErrorKind, AgentEventKind,
    AgentSessionEndpointError, CheckpointId, DurableOperationState, DurableOperationTransition,
    EventContext, StoreAdmission, TelemetryObservationKind,
};
use futures_util::StreamExt;
use plakous_model::{
    Digest, DigestDomain, ModelError, ModelErrorKind, ModelErrorPhase, ModelEvent,
    ModelOperationRoot, ModelResponse, ModelStreamItem, ProviderCallRecoveryLocatorStage,
};
use std::sync::Arc;

impl Driver {
    /// Persists the prepared request, dispatches it once, and settles the
    /// outcome: publish on success, terminalize on failure.
    pub(super) async fn dispatch_compaction(
        &mut self,
        attempt: &CompactionAttempt<'_>,
        runtime: ExactCompactionRuntime,
        measured: &MeasuredSource,
        prepared: PreparedCompaction,
        committed_detail: &mut CompactionDetail,
    ) -> Result<AgentCompactSessionOutcome, AgentSessionEndpointError> {
        let identity = attempt.identity;
        let source = attempt.source;
        let native_protected_history = prepared.native_protected_history;
        let prepared_record = prepared.prepared.record().clone();
        let target_affinity_entropy = self.compaction_target_affinity_entropy(identity)?;
        let record = DurableCompactionRequestRecord {
            version: prepared.record_version,
            workflow_id: identity.workflow_id,
            turn_fence: attempt.origin.is_turn_fence(),
            operation_id: attempt.request.operation_id.clone(),
            source_session: source.session_id,
            source_branch: source.branch_id,
            source_checkpoint: source.checkpoint_id,
            source_history: measured.projection.history().identity(),
            cut: prepared.cut_identity,
            request: prepared.request,
            provider_configuration: runtime.provider_configuration.as_ref().clone(),
            source_harness_plan: measured.harness_plan,
            before_measurement: measured.before.measurement_identity(),
            before_status: measured.before.status_identity(),
            before_input_tokens: measured.before.input_tokens(),
            target_affinity_entropy,
        };
        let (compaction_operation, provider_operation) = self
            .prepare_compaction_operations(identity, &record, &prepared_record)
            .await
            .map_err(|error| compact_runtime_endpoint_error(&error))?;
        let locator_refusal = match prepared_record.recovery_locator() {
            None => None,
            Some(prepared_locator) => match self
                .persist_compaction_locator(
                    identity,
                    provider_operation,
                    identity.prepared_locator_checkpoint,
                    prepared_locator,
                    ProviderCallRecoveryLocatorStage::Prepared,
                )
                .await
            {
                Ok(()) => None,
                // An ordinary refusal leaves both operations `Prepared`, so
                // the failure below settles them as proven not dispatched.
                Err(error) if ordinary_capacity_refusal(&error).is_some() => Some(error),
                Err(_) => {
                    return Ok(AgentCompactSessionOutcome::RecoveryRequired {
                        checkpoint: self.authority.checkpoint().id(),
                        diagnostic: None,
                    });
                }
            },
        };
        let mut live = LiveCompaction::prepared(
            attempt.origin,
            attempt.active.cloned(),
            identity,
            (compaction_operation, provider_operation),
            record,
            prepared.prepared,
            prepared.validated,
            runtime,
        );
        let result = if let Some(error) = locator_refusal {
            Err(error)
        } else {
            if attempt.origin.pressure_forced() {
                self.turn_budgets.pressure_fold_spent = true;
            }
            committed_detail.idle_gap = self.note_provider_request();
            Box::pin(self.run_compaction_call(&mut live)).await
        };
        match result {
            Ok(response) => {
                self.settle_compaction_response(
                    attempt,
                    measured,
                    &mut live,
                    &response,
                    native_protected_history,
                    committed_detail,
                )
                .await
            }
            Err(error) => {
                self.settle_compaction_failure(
                    attempt,
                    measured,
                    &mut live,
                    &error,
                    native_protected_history,
                    committed_detail,
                )
                .await
            }
        }
    }

    /// Fresh entropy for the successor checkpoint's cache affinity.
    fn compaction_target_affinity_entropy(
        &mut self,
        identity: DurableCompactionIdentity,
    ) -> Result<[u8; 32], AgentSessionEndpointError> {
        let entropy_left = self
            .ids
            .next_uuid(self.clock.now())
            .map_err(compact_runtime_endpoint_rejected)?;
        let entropy_right = self
            .ids
            .next_uuid(self.clock.now())
            .map_err(compact_runtime_endpoint_rejected)?;
        Ok(*Digest::sha256(
            DigestDomain::ProviderOperation,
            3,
            &[
                b"plakous-agent/session-compaction-affinity-v1",
                identity.workflow_id.as_bytes(),
                entropy_left.as_bytes(),
                entropy_right.as_bytes(),
            ],
        )
        .map_err(compact_runtime_endpoint_rejected)?
        .bytes())
    }

    /// The compaction summary is a model request too: it waits for a slot in
    /// the host's shared budget before its own deadline starts.
    async fn take_compaction_request_slot(
        &mut self,
        cancellation: plakous_model::CancellationToken,
    ) -> Result<Option<RequestSlot>, AgentError> {
        let Some(slots) = self.request_slots.clone() else {
            return Ok(None);
        };
        if let Some(slot) = slots.try_take() {
            return Ok(Some(slot));
        }
        if let Some(root) = &self.active_root {
            let _ = root.shared.events.publish(
                EventContext::turn(root.turn_id),
                AgentEventKind::ModelRequestQueued {
                    slots: slots.label().to_owned(),
                },
            );
        }
        tokio::select! {
            slot = slots.take() => Ok(Some(slot)),
            () = self.authority.lease_lost() => Err(self.authority.heartbeat_failure()),
            () = cancellation.cancelled() => Err(cancelled_error()),
            () = self.shutdown.cancelled() => Err(cancelled_error()),
        }
    }

    async fn run_compaction_call(
        &mut self,
        live: &mut LiveCompaction,
    ) -> Result<ModelResponse, AgentError> {
        let result = Box::pin(self.run_compaction_call_inner(live)).await;
        self.request_slot.take();
        result
    }

    #[allow(
        clippy::too_many_lines,
        reason = "The compaction dispatch sequence keeps preparation, dispatch fencing, response classification, and cleanup evidence together"
    )]
    async fn run_compaction_call_inner(
        &mut self,
        live: &mut LiveCompaction,
    ) -> Result<ModelResponse, AgentError> {
        self.wait_for_placement(live.cancellation().clone()).await?;
        self.request_slot = self
            .take_compaction_request_slot(live.cancellation().clone())
            .await?;
        let wait_elapsed_before = self.credential_wait_elapsed;
        let held_before = self.placement_wait_elapsed;
        let (base_deadline, context) = live.begin_call()?;
        let mut deadline_at;
        let mut slot_wait_elapsed = std::time::Duration::ZERO;
        let identity = live.identity();
        let (compaction_operation, provider_operation) = live.operations();
        let cancellation = live.cancellation().clone();
        let prepared_record = live.prepared_record().clone();
        let plan = live.plan().clone();
        let model = Arc::clone(&live.runtime().model);
        let provider_configuration = Arc::clone(&live.runtime().provider_configuration);
        let descriptor = provider_configuration.effective().descriptor().clone();
        let start = loop {
            self.wait_for_placement(cancellation.clone()).await?;
            if self.request_slot.is_none() {
                let queued_at = crate::executor::now();
                self.request_slot = self
                    .take_compaction_request_slot(cancellation.clone())
                    .await?;
                slot_wait_elapsed = slot_wait_elapsed.saturating_add(queued_at.elapsed());
            }
            deadline_at = base_deadline
                + self
                    .credential_wait_elapsed
                    .saturating_sub(wait_elapsed_before)
                + self.placement_wait_elapsed.saturating_sub(held_before)
                + slot_wait_elapsed;
            let provider_dispatch_gate = Arc::clone(&self.provider_dispatch_gate);
            let dispatch_guard = provider_dispatch_gate.lock().await;
            if self.placement_unavailable() {
                drop(dispatch_guard);
                continue;
            }
            if self.shutdown.is_cancelled() || cancellation.is_cancelled() {
                return Err(cancelled_error());
            }
            if crate::executor::now() >= deadline_at {
                return Err(self.model_timeout_error());
            }
            self.authority.check_heartbeat()?;
            let dispatch_checked_at = self.clock.now();
            let dispatch_authority = match Arc::clone(&model)
                .revalidate_runtime_authority(dispatch_checked_at)
                .await
            {
                Ok(authority) => authority,
                Err(plakous_model::RuntimeModelAuthorityError::AwaitingCredential) => {
                    drop(dispatch_guard);
                    self.wait_for_credential(Arc::clone(&model), cancellation.clone())
                        .await?;
                    continue;
                }
                Err(error) => return Err(self.authority_error(error)),
            };
            validate_configuration_runtime_authority(
                &provider_configuration,
                &descriptor,
                &dispatch_authority,
                dispatch_checked_at,
            )
            .map_err(ModelError::from)
            .map_err(|error| self.project_model_error(error, ModelErrorPhase::Preparation))?;
            prepared_record
                .revalidate_first_dispatch(
                    &descriptor,
                    &plan,
                    &dispatch_authority,
                    dispatch_checked_at,
                )
                .map_err(ModelError::from)
                .map_err(|error| self.project_model_error(error, ModelErrorPhase::Preparation))?;
            self.transition_compaction_operations(
                identity,
                compaction_operation,
                provider_operation,
                identity.dispatched_checkpoint,
                DurableOperationState::Dispatched,
            )
            .await?;
            let _lease_guard = self.start_authority().admit().await?;
            if self.shutdown.is_cancelled() || cancellation.is_cancelled() {
                return Err(cancelled_error());
            }
            if crate::executor::now() >= deadline_at {
                return Err(self.model_timeout_error());
            }
            let mut start = live.take_prepared()?.start_with_control(context);
            let mut task_context = std::task::Context::from_waker(std::task::Waker::noop());
            break match start.as_mut().poll(&mut task_context) {
                std::task::Poll::Pending => ModelStartAdmission::Pending(start),
                std::task::Poll::Ready(result) => ModelStartAdmission::Ready(result),
            };
        };
        let mut timeout = Box::pin(crate::executor::sleep_until(deadline_at));
        let mut stream = match start {
            ModelStartAdmission::Ready(result) => result.map_err(compaction_provider_error)?,
            ModelStartAdmission::Pending(mut start) => loop {
                tokio::select! {
                    () = self.authority.lease_lost() => {
                        cancellation.cancel();
                        let _ = join_cancelled_control_start(start, self.config.model_cleanup_timeout).await;
                        return Err(ambiguous_compaction_error("branch lease was lost after compaction dispatch"));
                    }
                    () = cancellation.cancelled() => {
                        let _ = join_cancelled_control_start(start, self.config.model_cleanup_timeout).await;
                        return Err(abandoned_compaction_error(AgentErrorKind::Cancelled, "compaction was abandoned after dispatch because the turn was cancelled; it was not applied and may have been billed"));
                    }
                    () = self.shutdown.cancelled() => {
                        cancellation.cancel();
                        let _ = join_cancelled_control_start(start, self.config.model_cleanup_timeout).await;
                        return Err(ambiguous_compaction_error("runtime stopped after compaction dispatch"));
                    }
                    () = &mut timeout => {
                        cancellation.cancel();
                        let _ = join_cancelled_control_start(start, self.config.model_cleanup_timeout).await;
                        return Err(self.abandoned_compaction_timeout());
                    }
                    control = self.control_receiver.recv() => {
                        let Some(control) = control else {
                            cancellation.cancel();
                            let _ = join_cancelled_control_start(start, self.config.model_cleanup_timeout).await;
                            return Err(abandoned_compaction_error(AgentErrorKind::Cancelled, "compaction was abandoned after dispatch because the runtime control closed; it was not applied and may have been billed"));
                        };
                        Box::pin(self.handle_control(live.active(), control)).await;
                    }
                    result = &mut start => break result.map_err(compaction_provider_error)?,
                }
            },
        };
        loop {
            let next = tokio::select! {
                () = self.authority.lease_lost() => {
                    cancellation.cancel();
                    let _ = join_cancelled_control_stream(&mut stream, self.config.model_cleanup_timeout).await;
                    return Err(ambiguous_compaction_error("branch lease was lost after compaction dispatch"));
                }
                () = cancellation.cancelled() => {
                    let _ = join_cancelled_control_stream(&mut stream, self.config.model_cleanup_timeout).await;
                    return Err(abandoned_compaction_error(AgentErrorKind::Cancelled, "compaction was abandoned after dispatch because the turn was cancelled; it was not applied and may have been billed"));
                }
                () = self.shutdown.cancelled() => {
                    cancellation.cancel();
                    let _ = join_cancelled_control_stream(&mut stream, self.config.model_cleanup_timeout).await;
                    return Err(ambiguous_compaction_error("runtime stopped after compaction dispatch"));
                }
                control = self.control_receiver.recv() => {
                    let Some(control) = control else {
                        cancellation.cancel();
                        let _ = join_cancelled_control_stream(&mut stream, self.config.model_cleanup_timeout).await;
                        return Err(abandoned_compaction_error(AgentErrorKind::Cancelled, "compaction was abandoned after dispatch because the runtime control closed; it was not applied and may have been billed"));
                    };
                    Box::pin(self.handle_control(live.active(), control)).await;
                    continue;
                }
                item = stream.next() => item,
            };
            let Some(item) = next else { break };
            match item.map_err(compaction_provider_error)? {
                ModelStreamItem::RecoveryLocatorBound(locator) => {
                    let locator = match live.validate_locator(locator) {
                        Ok(locator) => locator,
                        Err(error) => {
                            cancellation.cancel();
                            let _ = join_cancelled_control_stream(
                                &mut stream,
                                self.config.model_cleanup_timeout,
                            )
                            .await;
                            return Err(error);
                        }
                    };
                    if self
                        .persist_compaction_locator(
                            identity,
                            provider_operation,
                            identity.learned_locator_checkpoint,
                            locator.locator(),
                            ProviderCallRecoveryLocatorStage::LearnedAfterDispatch,
                        )
                        .await
                        .is_err()
                    {
                        cancellation.cancel();
                        let _ = join_cancelled_control_stream(
                            &mut stream,
                            self.config.model_cleanup_timeout,
                        )
                        .await;
                        return Err(ambiguous_compaction_error(
                            "learned compaction recovery locator was not durably bound",
                        ));
                    }
                    live.locator_published(locator);
                }
                ModelStreamItem::Event(event) => {
                    if let ModelEvent::PromptCacheDiagnosis(diagnosis) = &event {
                        self.emit_telemetry(
                            self.telemetry_context_for_turn(live.turn_id())
                                .with_attempt(live.identity().attempt_id),
                            TelemetryObservationKind::PromptCacheDiagnosis {
                                reason: diagnosis.reason,
                                missed_input_tokens: diagnosis.missed_input_tokens,
                            },
                        );
                    }
                    live.observe_event(event).map_err(|error| match error {
                        CompactionStreamError::ContradictoryTerminal => ambiguous_compaction_error(
                            "compaction provider contradicted an observed terminal response",
                        ),
                        CompactionStreamError::ForeignPlan => self.model_failure(
                            ModelErrorKind::InvalidEvent,
                            "compaction response belongs to another prepared plan",
                            ModelErrorPhase::Terminal,
                        ),
                        CompactionStreamError::InvalidTerminal(error) => {
                            self.project_model_error(error, ModelErrorPhase::Terminal)
                        }
                    })?;
                }
            }
        }
        live.finish_stream()
    }

    pub(super) async fn transition_compaction_operations(
        &mut self,
        identity: DurableCompactionIdentity,
        compaction_operation: OperationId,
        provider_operation: OperationId,
        checkpoint_id: CheckpointId,
        state: DurableOperationState,
    ) -> Result<(), AgentError> {
        let mut admission = StoreAdmission::Ordinary;
        for operation in [compaction_operation, provider_operation] {
            if transition_admission(&self.operation_state(operation)?, &state)
                != StoreAdmission::Ordinary
            {
                admission = StoreAdmission::Settlement;
            }
        }
        let (history, mut ledger, context, family) = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            (
                manifest.canonical().history().clone(),
                manifest.ledger().clone(),
                manifest.canonical().context().retained_for_exact_resume(),
                manifest.family_artifact().cloned(),
            )
        };
        for operation in [compaction_operation, provider_operation] {
            ledger = ledger
                .transition_operation(
                    operation.0,
                    DurableOperationTransition::new(checkpoint_id, state.clone()),
                )
                .map_err(runtime_manifest_error)?;
        }
        let lifecycle = working_lifecycle_for_root(
            ModelOperationRoot::SessionWorkflow(identity.workflow_id),
            &ledger,
        )?;
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
        Ok(())
    }
}
