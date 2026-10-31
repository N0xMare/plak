//! Mandatory aggregate-image admission, independent of optional pressure folds.

use crate::runtime::PromptCommand;
use crate::runtime::compaction::record::compaction_operation_component;
use crate::runtime::compaction::residency::record::{
    ResidencyEvidence, ResidencyRequest, operation_identity, publication,
};
use crate::runtime::compaction::residency::{ImageTotals, evaluate, residency_successor};
use crate::runtime::compaction::{DurableCompactionIdentity, ExactCompactionRuntime};
use crate::runtime::continuation::runtime_manifest_error;
use crate::runtime::driver::Driver;
use crate::runtime::driver::compaction::MANUAL_COMPACTION_REMEDY;
use crate::runtime::durable::DurableSessionAddress;
use crate::{
    AgentError, AgentErrorKind, AgentEventKind, CompactionEventOutcome, CompactionEventReason,
    DurableOperationKind, DurableOperationRecord, DurableOperationState,
    DurableOperationTransition, EventContext, StoreAdmission, TelemetryObservationKind,
};
use plakous_model::{ContentBlock, ContextStatusKind, ResidentImageBudget};
use std::sync::Arc;

impl Driver {
    pub(in crate::runtime::driver) async fn require_image_residency(
        &mut self,
        command: &PromptCommand,
    ) -> Result<(), AgentError> {
        self.maybe_image_residency(command, false).await?;
        if self.resident_images_fit() {
            return Ok(());
        }
        if !self.config.scheduled_compaction_enabled {
            return Err(AgentError::new(
                AgentErrorKind::RequestBudget,
                format!(
                    "resident image request budget exceeded: no residency successor fits this request's context; {MANUAL_COMPACTION_REMEDY}"
                ),
            ));
        }
        // The pending prompt may itself have crossed the token limit after
        // admission measured the committed source. Give the ladder that cut
        // too; its existing paid-fold budget prevents a second paid dispatch.
        self.compact_after_deferred_image_residency(command).await?;
        self.maybe_image_residency(command, false).await?;
        if !self.resident_images_fit() {
            return Err(AgentError::new(
                AgentErrorKind::RequestBudget,
                "resident image request budget exceeded: no residency successor fits this request's context, and ordinary compaction did not make room",
            ));
        }
        Ok(())
    }

    fn resident_images_fit(&self) -> bool {
        crate::runtime::compaction::residency::image_totals(
            self.authority.checkpoint().manifest().canonical().history(),
            ImageTotals::ZERO,
        )
        .fits(ResidentImageBudget::default())
    }

    pub(in crate::runtime::driver) async fn maybe_image_residency(
        &mut self,
        command: &PromptCommand,
        admission: bool,
    ) -> Result<(), AgentError> {
        let checkpoint = self.authority.checkpoint();
        if !checkpoint
            .manifest()
            .canonical()
            .working()
            .segments()
            .is_empty()
        {
            return Ok(());
        }
        let mut incoming = ImageTotals::ZERO;
        if admission {
            for block in command.input.content_blocks().map_err(|_| {
                AgentError::new(
                    AgentErrorKind::InvalidInput,
                    "pending image input is invalid",
                )
            })? {
                if let ContentBlock::Image(image) = block {
                    incoming.add_image(&image);
                }
            }
        }
        let Some(plan) = evaluate(
            checkpoint.manifest().canonical().history(),
            incoming,
            ResidentImageBudget::default(),
        )?
        else {
            return Ok(());
        };
        self.reresolve_expired_model(command).await?;
        self.publish_image_residency(command, admission, plan).await
    }

    async fn publish_image_residency(
        &mut self,
        command: &PromptCommand,
        admission: bool,
        plan: crate::runtime::compaction::residency::ResidencyPlan,
    ) -> Result<(), AgentError> {
        let source = self.authority.checkpoint();
        let address = DurableSessionAddress::new(
            source.manifest().topology().session_id,
            source.manifest().topology().branch_id,
            source.id(),
        );
        // A deferred pass can inspect the same checkpoint again. Each fresh
        // preparation needs its own attempt identity, even without publication.
        let uuid = self
            .ids
            .next_uuid(self.clock.now())
            .map_err(|error| AgentError::from_identity(&error))?;
        let operation_id = format!("residency:{}:{uuid}", command.turn_id);
        let identity = DurableCompactionIdentity::new(&operation_id, address)?;
        let successor = residency_successor(
            source.manifest().canonical().history(),
            &plan,
            identity.workflow_id,
        )?;
        let runtime = ExactCompactionRuntime {
            model: Arc::clone(&self.model),
            provider_configuration: Arc::clone(&self.provider_configuration),
            clock: Arc::clone(&self.clock),
            model_operation_timeout: self.config.model_operation_timeout,
        };
        let (before_history, after_history) = measurement_histories(
            &source,
            &successor.history,
            admission.then_some(&command.input),
        )?;
        // Both sides prepare fresh, on the same unanchored basis. A source
        // estimate from a former dispatch is never a shrink certificate.
        let (_, before, before_wire) = self
            .measure_placeholder_successor_with_wire_digest(&runtime, identity, 0, &before_history)
            .await?;
        let (status, after, after_wire) = self
            .measure_placeholder_successor_with_wire_digest(&runtime, identity, 1, &after_history)
            .await?;
        // Image retirement must not preempt a text-pressure fold. The ordinary
        // ladder may make this plan unnecessary or make a later pass fit.
        if !matches!(status.kind(), ContextStatusKind::Fits { .. }) {
            return Ok(());
        }
        if before_wire.is_none()
            || after_wire.is_none()
            || after.input_tokens() >= before.input_tokens()
        {
            return Err(AgentError::new(
                AgentErrorKind::RequestBudget,
                "resident image request budget exceeded: no independently prepared, fitting and strictly smaller residency successor",
            ));
        }
        let request = ResidencyRequest {
            version: 3,
            workflow_id: identity.workflow_id,
            operation_id,
            source_session: address.session_id,
            source_branch: address.branch_id,
            source_checkpoint: source.id(),
            source_history: source.manifest().canonical().history().identity(),
            source_projection: source.manifest().canonical().projection().identity(),
            source_harness_plan: source
                .manifest()
                .canonical()
                .stable_prefix()
                .resolved_harness_plan_identity()
                .ok_or_else(|| {
                    AgentError::new(
                        AgentErrorKind::Session,
                        "image residency source has no harness plan",
                    )
                })?,
            admission,
            plan,
            before_measurement: before.measurement_identity(),
            before_status: before.status_identity(),
            before_input_tokens: before.input_tokens(),
        };
        let evidence = ResidencyEvidence {
            version: 3,
            workflow_id: identity.workflow_id,
            source_checkpoint: source.id(),
            operation: operation_identity(
                identity.workflow_id,
                &request.plan,
                request.source_history,
                &successor,
            )?,
            selection: request.plan.identity()?,
            successor_history: successor.history.identity(),
            notice_segment: successor.notice.identity(),
            replaced_images: crate::runtime::compaction::residency::checked_count(
                request.plan.selection.len(),
            )?,
            before: request.plan.before,
            after: request.plan.after,
            after_measurement: after.measurement_identity(),
            after_status: after.status_identity(),
            after_input_tokens: after.input_tokens(),
        };
        self.commit_image_residency(command, identity, request, evidence, successor)
            .await
    }

    async fn commit_image_residency(
        &mut self,
        command: &PromptCommand,
        identity: DurableCompactionIdentity,
        request: ResidencyRequest,
        evidence: ResidencyEvidence,
        successor: crate::runtime::compaction::residency::ResidencySuccessor,
    ) -> Result<(), AgentError> {
        let source = self.authority.checkpoint();
        let context = source
            .manifest()
            .canonical()
            .context()
            .retained_for_exact_resume()
            .successful_image_residency(
                &publication(&source, &request, &evidence)?,
                &successor.notice,
                &successor.history,
                request.selection_audit(crate::runtime::compaction::residency::checked_count(
                    source.manifest().canonical().history().segments().len(),
                )?)?,
            )
            .map_err(runtime_manifest_error)?;
        let request_component = compaction_operation_component(&request)?;
        let evidence_component = compaction_operation_component(&evidence)?;
        let operation = self.take_operation_id()?;
        let ledger = source
            .manifest()
            .ledger()
            .clone()
            .append_operation(
                DurableOperationRecord::new(
                    operation.0,
                    DurableOperationKind::Compaction {
                        workflow_id: identity.workflow_id,
                    },
                    request_component,
                    vec![DurableOperationTransition::new(
                        identity.success_checkpoint,
                        DurableOperationState::Finalized(evidence_component),
                    )],
                )
                .map_err(runtime_manifest_error)?,
            )
            .map_err(runtime_manifest_error)?;
        self.context_anchor = None;
        self.request_anchor = None;
        self.admitted_round = None;
        self.publish_checkpoint(
            StoreAdmission::Ordinary,
            identity.success_checkpoint,
            source.manifest().lifecycle().clone(),
            successor.history,
            ledger,
            context,
            None,
        )
        .await?;
        self.last_compaction_successor = Some(evidence.successor_history);
        let context = EventContext::turn(command.turn_id)
            .with_feed_seq(self.authority.feed_seq_at(identity.success_checkpoint));
        let _ = command.shared.events.publish(
            context,
            AgentEventKind::CompactionStarted {
                workflow_id: identity.workflow_id,
                reason: CompactionEventReason::Residency,
            },
        );
        let _ = command.shared.events.publish(
            context,
            AgentEventKind::CompactionFinished {
                workflow_id: identity.workflow_id,
                outcome: CompactionEventOutcome::Committed,
                elapsed_ms: 0,
                summary: crate::runtime::observation::committed_compaction_summary(&self.authority),
                detail: None,
            },
        );
        self.emit_telemetry(
            self.telemetry_context.with_turn(command.turn_id),
            TelemetryObservationKind::CompactionFinished {
                refusal: None,
                elapsed: std::time::Duration::ZERO,
                outcome: CompactionEventOutcome::Committed,
                reason: CompactionEventReason::Residency,
                tokens_before: Some(request.before_input_tokens),
                tokens_after: Some(evidence.after_input_tokens),
                usage: None,
            },
        );
        Ok(())
    }
}

fn measurement_histories(
    source: &crate::RuntimeCheckpoint,
    successor: &plakous_model::CanonicalHistory,
    incoming: Option<&crate::input::UserInput>,
) -> Result<
    (
        plakous_model::CanonicalHistory,
        plakous_model::CanonicalHistory,
    ),
    AgentError,
> {
    let before = source.manifest().canonical().history().clone();
    let Some(input) = incoming.filter(|input| !input.parts().is_empty()) else {
        return Ok((before, successor.clone()));
    };
    let invalid = || {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "pending image measurement projection is invalid",
        )
    };
    let pending =
        crate::runtime::context::pending_user_segment(source, input).map_err(|_| invalid())?;
    Ok((
        before.append(pending.clone()).map_err(|_| invalid())?,
        successor.append(pending).map_err(|_| invalid())?,
    ))
}
