//! Measuring a compaction source and its candidate successors.

use super::{CompactionAttempt, CompactionDetail, CompactionFitBasis, MeasuredSource};
use crate::runtime::admission::validate_configuration_runtime_authority;
use crate::runtime::compaction::fold::compact_diagnostic;
use crate::runtime::compaction::record::compaction_attempt_id;
use crate::runtime::compaction::request::{
    compact_runtime_endpoint_error, compact_runtime_endpoint_rejected, context_status_audit,
    native_compaction_facet_enrolled, unmeasured_native_compact_audit,
};
use crate::runtime::compaction::{DurableCompactionIdentity, ExactCompactionRuntime};
use crate::runtime::context::context_status_projection;
use crate::runtime::driver::Driver;
use crate::{
    AgentCompactSessionRequest, AgentCompactionReason, AgentError, AgentErrorKind,
    AgentSessionEndpointError, ContextEvaluationAudit, RuntimeCheckpoint,
};
use plakous_model::{
    ContextStatus, ContextStatusKind, ConversationView, Digest, ModelCacheAffinity, ModelError,
    ModelErrorPhase, ModelPreparationContext, ModelPreparationError, ModelRequest, RequestMetadata,
    SemanticCompactionCut, Timestamp, ValidationMode,
};
use std::sync::Arc;

impl Driver {
    /// Measures the current head as this pass's compaction source and picks its rung.
    pub(super) async fn measure_compaction_source(
        &mut self,
        attempt: &CompactionAttempt<'_>,
        runtime: &ExactCompactionRuntime,
        native_allowed: bool,
        measured_source_tokens: &mut Option<u64>,
        committed_detail: &mut CompactionDetail,
    ) -> Result<MeasuredSource, AgentSessionEndpointError> {
        let request = attempt.request;
        let checkpoint = self.authority.checkpoint();
        let cache_affinity = checkpoint
            .manifest()
            .canonical()
            .context()
            .affinity()
            .clone();
        let projection = checkpoint.manifest().canonical().projection();
        let (source_request, qualification_request) = self.compaction_measurement_requests(
            &checkpoint,
            &projection,
            request,
            &attempt.identity,
            runtime,
            native_allowed,
        )?;
        let native =
            native_allowed && native_compaction_facet_enrolled(&runtime.provider_configuration);
        committed_detail.rung = Some(if native {
            "native facet"
        } else {
            "harness summary"
        });
        let scheduled_pending_qualified = if request.reason == AgentCompactionReason::Scheduled
            && let Some(qualification_request) = qualification_request
        {
            let qualification_status = self
                .measure_compaction_projection(
                    runtime,
                    qualification_request,
                    CompactionFitBasis::ActiveHead,
                    None,
                    runtime.clock.now(),
                )
                .await
                .map_err(|error| compact_runtime_endpoint_error(&error))?
                .0;
            !matches!(qualification_status.kind(), ContextStatusKind::Fits { .. })
        } else {
            false
        };
        let measured = self
            .measure_compaction_projection(
                runtime,
                source_request,
                CompactionFitBasis::ActiveHead,
                None,
                runtime.clock.now(),
            )
            .await;
        let (before, occupancy_before, fits) = match measured {
            Ok((source_status, before)) => (
                before,
                source_status.unanchored_occupancy_tokens(),
                matches!(source_status.kind(), ContextStatusKind::Fits { .. }),
            ),
            Err(error)
                if request.reason == AgentCompactionReason::Scheduled
                    && request.pending_user_input.is_some()
                    && !scheduled_pending_qualified =>
            {
                // Skip-if-fits is idle-only: a pending user that still Fits must
                // not start a paid compact when source measurement fails.
                // Fence (b) has no pending user; measurement failure must not
                // become Unavailable (WouldOverflow / mid-turn source).
                return Err(compact_runtime_endpoint_error(&error));
            }
            Err(_) if native || attempt.origin.is_turn_fence() => (
                unmeasured_native_compact_audit(projection.identity())
                    .map_err(|error| compact_runtime_endpoint_error(&error))?,
                None,
                true,
            ),
            Err(error) => {
                return Err(compact_runtime_endpoint_rejected(format!(
                    "compaction source measurement failed: {}",
                    compact_diagnostic(&error)
                )));
            }
        };
        *measured_source_tokens = Some(before.input_tokens());
        let harness_plan = checkpoint
            .manifest()
            .canonical()
            .stable_prefix()
            .resolved_harness_plan_identity()
            .ok_or(AgentSessionEndpointError::Rejected {
                reason: "compaction source omitted its harness plan identity".into(),
            })?;
        Ok(MeasuredSource {
            checkpoint,
            cache_affinity,
            projection,
            harness_plan,
            before,
            occupancy_before,
            fits,
            native,
            scheduled_pending_qualified,
        })
    }

    /// The source measurement request and, for a pending user, the request that
    /// qualifies a scheduled compaction.
    fn compaction_measurement_requests(
        &mut self,
        checkpoint: &RuntimeCheckpoint,
        source_projection: &ConversationView,
        request: &AgentCompactSessionRequest,
        identity: &DurableCompactionIdentity,
        runtime: &ExactCompactionRuntime,
        native_allowed: bool,
    ) -> Result<(ModelRequest, Option<ModelRequest>), AgentSessionEndpointError> {
        let source_request = ModelRequest::from_parts(
            checkpoint.manifest().canonical().stable_prefix().clone(),
            source_projection.clone(),
            RequestMetadata::for_session_workflow(
                self.run_id,
                identity.workflow_id,
                compaction_attempt_id(
                    identity.workflow_id,
                    if native_allowed {
                        b"source-measurement"
                    } else {
                        b"summary-source-measurement"
                    },
                )
                .map_err(|error| compact_runtime_endpoint_error(&error))?,
                runtime.clock.now(),
            ),
        )
        .map_err(compact_runtime_endpoint_rejected)?;
        let qualification_request = request
            .pending_user_input
            .as_ref()
            .map(|pending| {
                let notice = self
                    .process_notices()
                    .map_err(|_| AgentSessionEndpointError::Unavailable)?
                    .map(|(segment, _)| segment);
                let projection = context_status_projection(checkpoint, Some(pending), notice)?;
                ModelRequest::from_parts(
                    checkpoint.manifest().canonical().stable_prefix().clone(),
                    projection,
                    RequestMetadata::for_session_workflow(
                        self.run_id,
                        identity.workflow_id,
                        compaction_attempt_id(
                            identity.workflow_id,
                            if native_allowed {
                                b"qualification-measurement"
                            } else {
                                b"summary-qualification-measurement"
                            },
                        )
                        .map_err(|error| compact_runtime_endpoint_error(&error))?,
                        runtime.clock.now(),
                    ),
                )
                .map_err(compact_runtime_endpoint_rejected)
            })
            .transpose()?;
        Ok((source_request, qualification_request))
    }

    /// Whether the cut's retained tail alone is definitely over the window.
    ///
    /// Measured like a successor (fresh, without the live request anchor). A
    /// measurement that cannot be made answers `false`, leaving the paid fold
    /// and its own successor check to decide.
    pub(super) async fn retained_tail_cannot_fit(
        &mut self,
        runtime: &ExactCompactionRuntime,
        stable_prefix: &plakous_model::StableRequestPrefix,
        cut: &SemanticCompactionCut,
        pending: Option<plakous_model::HistorySegment>,
        identity: DurableCompactionIdentity,
        affinity: ModelCacheAffinity,
    ) -> bool {
        let mut segments = cut.retained_segments().to_vec();
        segments.extend(pending);
        let Ok(tail) = plakous_model::CanonicalHistory::new(segments) else {
            return false;
        };
        let Ok(attempt) = compaction_attempt_id(identity.workflow_id, b"retained-tail-fit") else {
            return false;
        };
        let Ok(request) = ModelRequest::from_parts(
            stable_prefix.clone(),
            ConversationView::from_shared(tail, plakous_model::WorkingProjection::default()),
            RequestMetadata::for_session_workflow(
                self.run_id,
                identity.workflow_id,
                attempt,
                runtime.clock.now(),
            ),
        ) else {
            return false;
        };
        self.measure_compaction_projection(
            runtime,
            request,
            CompactionFitBasis::OfflineSuccessor,
            Some(affinity),
            runtime.clock.now(),
        )
        .await
        .is_ok_and(|(status, _)| !matches!(status.kind(), ContextStatusKind::Fits { .. }))
    }

    pub(in crate::runtime::driver) async fn measure_compaction_projection(
        &mut self,
        runtime: &ExactCompactionRuntime,
        request: ModelRequest,
        basis: CompactionFitBasis,
        explicit_affinity: Option<ModelCacheAffinity>,
        prepared_at: Timestamp,
    ) -> Result<(ContextStatus, ContextEvaluationAudit), AgentError> {
        self.measure_compaction_projection_with_wire_digest(
            runtime,
            request,
            basis,
            explicit_affinity,
            prepared_at,
        )
        .await
        .map(|(status, audit, _)| (status, audit))
    }

    pub(in crate::runtime::driver) async fn measure_compaction_projection_with_wire_digest(
        &mut self,
        runtime: &ExactCompactionRuntime,
        request: ModelRequest,
        basis: CompactionFitBasis,
        explicit_affinity: Option<ModelCacheAffinity>,
        prepared_at: Timestamp,
    ) -> Result<(ContextStatus, ContextEvaluationAudit, Option<Digest>), AgentError> {
        let descriptor = runtime
            .provider_configuration
            .effective()
            .descriptor()
            .clone();
        let plan = descriptor
            .validate_request(request, ValidationMode::Strict)
            .map_err(ModelError::from)
            .map_err(|error| self.project_model_error(error, ModelErrorPhase::Preparation))?;
        descriptor
            .preflight_context_admission_requirement(plan.request().context_admission_requirement())
            .map_err(ModelPreparationError::from)
            .map_err(ModelError::from)
            .map_err(|error| self.project_model_error(error, ModelErrorPhase::Preparation))?;
        let (authority, prepared_at) = self
            .request_authority(Arc::clone(&runtime.model), prepared_at)
            .await?;
        validate_configuration_runtime_authority(
            &runtime.provider_configuration,
            &descriptor,
            &authority,
            prepared_at,
        )
        .map_err(ModelError::from)
        .map_err(|error| self.project_model_error(error, ModelErrorPhase::Preparation))?;
        let mut context = self.with_completed_history_prefix(
            &plan,
            ModelPreparationContext::new(authority, prepared_at).with_request_anchor(
                if basis == CompactionFitBasis::OfflineSuccessor {
                    None
                } else {
                    self.request_anchor.clone()
                },
            ),
        );
        if basis == CompactionFitBasis::ActiveHead {
            context = self.with_previous_response_metadata(&plan, context)?;
            let (artifact, affinity) = self.active_model_preparation_state()?;
            context = context.with_cache_affinity(affinity);
            if let Some(artifact) = artifact {
                context = context.with_active_family_artifact(artifact);
            }
        } else {
            if matches!(
                basis,
                CompactionFitBasis::PaidFoldInput | CompactionFitBasis::NativeFoldInput
            ) {
                context = context.with_required_input_bound();
            }
            if basis == CompactionFitBasis::NativeFoldInput {
                let (artifact, _) = self.active_model_preparation_state()?;
                if let Some(artifact) = artifact {
                    context = context.with_active_family_artifact(artifact);
                }
            }
            if let Some(affinity) = explicit_affinity {
                context = context.with_cache_affinity(affinity);
            }
        }
        let mut wire_digest = None;
        let status = match Arc::clone(&runtime.model).prepare_call(plan.clone(), context) {
            Ok(prepared) => {
                let record = prepared.record();
                wire_digest = Some(record.projection().wire_digest());
                descriptor
                    .context_status(&plan, record.projection(), prepared_at)
                    .map_err(|_| {
                        AgentError::new(
                            AgentErrorKind::Model,
                            "prepared context status could not be reconstructed",
                        )
                    })?
                    .with_input_parts(record.projection().token_projection().input_parts())
            }
            Err(error) => error.context_status().cloned().ok_or_else(|| {
                self.project_model_error(error.into(), ModelErrorPhase::Preparation)
            })?,
        };
        let audit = context_status_audit(&status)?;
        Ok((status, audit, wire_digest))
    }
}
