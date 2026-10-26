//! Selecting and preparing the one paid compaction request of a pass.

use super::{
    CompactionAttempt, CompactionDetail, CompactionFitBasis, CompactionPhase,
    CompactionRequestPlan, MeasuredSource, PreparedCompaction,
};
use crate::runtime::OperationId;
use crate::runtime::admission::validate_configuration_runtime_authority;
use crate::runtime::compaction::fold::compact_diagnostic;
use crate::runtime::compaction::record::{compaction_attempt_id, compaction_operation_component};
use crate::runtime::compaction::request::{
    compact_runtime_endpoint_error, compact_runtime_endpoint_rejected, harness_compaction_input,
    harness_compaction_input_at_depth, harness_compaction_prefix,
    harness_compaction_reduction_count, native_compaction_dispatch_prefix,
    portable_compaction_history,
};
use crate::runtime::compaction::{
    DurableCompactionIdentity, DurableCompactionRequestRecord, ExactCompactionRuntime,
};
use crate::runtime::context::pending_user_segment;
use crate::runtime::continuation::{runtime_component, runtime_manifest_error};
use crate::runtime::driver::Driver;
use crate::runtime::driver::operations::transition_admission;
use crate::runtime::operation_record::working_lifecycle_for_root;
use crate::{
    AgentCompactSessionOutcome, AgentError, AgentErrorKind, AgentSessionEndpointError,
    CheckpointId, DurableOperationKind, DurableOperationRecord, DurableOperationState,
    DurableOperationTransition, RuntimeComponentKind, StoreAdmission,
};
use plakous_model::{
    ContextHistoryPlan, ContextStatusKind, ConversationView, ModelCacheAffinity, ModelError,
    ModelErrorKind, ModelErrorPhase, ModelOperationRoot, ModelPreparationContext,
    ModelPreparationError, ModelRequest, PreparedCallRecord, ProviderCallRecoveryLocator,
    ProviderCallRecoveryLocatorStage, RequestMetadata, SemanticCompactionCut, ValidatedRequestPlan,
    ValidationMode,
};
use std::sync::Arc;

impl Driver {
    /// Selects the paid request for this pass: the native facet over the whole
    /// source, or the sealed harness cut at the shallowest fitting depth.
    pub(super) async fn select_compaction_request(
        &mut self,
        attempt: &CompactionAttempt<'_>,
        runtime: &ExactCompactionRuntime,
        measured: &MeasuredSource,
        committed_detail: &mut CompactionDetail,
    ) -> Result<CompactionPhase<CompactionRequestPlan>, AgentSessionEndpointError> {
        let identity = attempt.identity;
        let (mut plan, cut) = if measured.native {
            let prefix = native_compaction_dispatch_prefix(
                measured.checkpoint.manifest().canonical().stable_prefix(),
            )
            .map_err(|error| compact_runtime_endpoint_error(&error))?;
            let plan = CompactionRequestPlan {
                request: ModelRequest::from_parts(
                    prefix,
                    measured.projection.clone(),
                    RequestMetadata::for_session_workflow(
                        self.run_id,
                        identity.workflow_id,
                        identity.attempt_id,
                        runtime.clock.now(),
                    ),
                )
                .map_err(compact_runtime_endpoint_rejected)?,
                cut_identity: measured.projection.identity(),
                record_version: 3_u32,
            };
            (plan, None)
        } else {
            match self
                .harness_compaction_request(attempt, runtime, measured)
                .await?
            {
                CompactionPhase::Next((plan, cut)) => (plan, Some(cut)),
                CompactionPhase::RetryWithoutNative => {
                    return Ok(CompactionPhase::RetryWithoutNative);
                }
                CompactionPhase::Done(outcome) => return Ok(CompactionPhase::Done(outcome)),
            }
        };
        if measured.native && !measured.fits {
            let probe = ModelRequest::from_parts(
                plan.request.stable_prefix().clone(),
                plan.request.input().clone(),
                RequestMetadata::for_session_workflow(
                    self.run_id,
                    identity.workflow_id,
                    compaction_attempt_id(identity.workflow_id, b"native-fit")
                        .map_err(|error| compact_runtime_endpoint_error(&error))?,
                    runtime.clock.now(),
                ),
            )
            .map_err(compact_runtime_endpoint_rejected)?;
            if self
                .measure_compaction_projection(
                    runtime,
                    probe,
                    CompactionFitBasis::NativeFoldInput,
                    Some(measured.cache_affinity.clone()),
                    runtime.clock.now(),
                )
                .await
                .is_ok_and(|(status, _)| {
                    matches!(
                        status.kind(),
                        ContextStatusKind::WouldOverflow(_)
                            | ContextStatusKind::ProtectedOverflow(_)
                    )
                })
            {
                committed_detail.diagnostic = Some(
                    "native facet preparation refused for capacity; trying the sealed harness cut"
                        .into(),
                );
                return Ok(CompactionPhase::RetryWithoutNative);
            }
        }
        if let Some(cut) = cut {
            // With no tool results and an identical portable input there is
            // nothing to fit beyond the separately admitted dispatch request.
            let has_distinct_candidate = harness_compaction_reduction_count(&cut) > 1
                || harness_compaction_input_at_depth(&cut, identity.workflow_id, 1)
                    .map_err(|error| compact_runtime_endpoint_error(&error))?
                    != *plan.request.input();
            if has_distinct_candidate {
                self.fit_harness_summary_depth(
                    runtime,
                    identity.workflow_id,
                    &measured.cache_affinity,
                    &cut,
                    &mut plan.request,
                )
                .await?;
            }
        }
        Ok(CompactionPhase::Next(plan))
    }

    /// The harness summary request over the sealed cut of this pass's source,
    /// or the free rung when no summary can make a fitting successor.
    async fn harness_compaction_request(
        &mut self,
        attempt: &CompactionAttempt<'_>,
        runtime: &ExactCompactionRuntime,
        measured: &MeasuredSource,
    ) -> Result<
        CompactionPhase<(CompactionRequestPlan, SemanticCompactionCut)>,
        AgentSessionEndpointError,
    > {
        let identity = attempt.identity;
        let plan = ContextHistoryPlan::new(&measured.projection)
            .map_err(compact_runtime_endpoint_rejected)?;
        let cut = match SemanticCompactionCut::seal(&measured.projection, &plan) {
            Ok(cut) => cut,
            Err(plakous_model::SemanticCompactionCutError::NoEligibleSelectedPrefix) => {
                // Ladder rung three. The native facet is not enrolled on
                // this route and protection leaves no older intent group
                // to fold, which is the single-turn shape where the whole
                // session is the latest intent and its causal tail. Replace
                // the oldest completed tool-result bodies instead of
                // refusing; `NoEligibleRange` still comes back when even
                // that cannot fit, keeping the existing refusal.
                return Ok(CompactionPhase::Done(
                    self.compaction_source_placeholders(attempt, runtime, measured, false)
                        .await,
                ));
            }
            Err(error) => return Err(compact_runtime_endpoint_rejected(error)),
        };
        // A source that fits always has a tail that fits. When it does not,
        // a retained tail that cannot fit on its own means no summary can
        // make a fitting successor, so paying for one is waste: the free
        // rung runs instead, exactly as when nothing older is foldable.
        // At the idle fence the successor must also hold the pending
        // prompt, so a pending prompt that does not fit beside the source
        // runs the gate with that prompt after the tail.
        let pending_segment = if measured.scheduled_pending_qualified {
            attempt
                .request
                .pending_user_input
                .as_ref()
                .map(|input| pending_user_segment(&measured.checkpoint, input))
                .transpose()?
        } else {
            None
        };
        if (!measured.fits || pending_segment.is_some())
            && self
                .retained_tail_cannot_fit(
                    runtime,
                    measured.checkpoint.manifest().canonical().stable_prefix(),
                    &cut,
                    pending_segment,
                    identity,
                    measured.cache_affinity.clone(),
                )
                .await
        {
            return Ok(CompactionPhase::Done(
                self.compaction_source_placeholders(attempt, runtime, measured, false)
                    .await,
            ));
        }
        let prefix = harness_compaction_prefix(
            measured.checkpoint.manifest().canonical().stable_prefix(),
            runtime.provider_configuration.selected_identity(),
            runtime.provider_configuration.effective_identity(),
        )
        .map_err(|error| compact_runtime_endpoint_error(&error))?;
        let input = harness_compaction_input(&cut, identity.workflow_id)
            .map_err(|error| compact_runtime_endpoint_error(&error))?;
        let plan = CompactionRequestPlan {
            request: ModelRequest::from_parts(
                prefix,
                input,
                RequestMetadata::for_session_workflow(
                    self.run_id,
                    identity.workflow_id,
                    identity.attempt_id,
                    runtime.clock.now(),
                ),
            )
            .map_err(compact_runtime_endpoint_rejected)?,
            cut_identity: cut.identity(),
            record_version: 2_u32,
        };
        Ok(CompactionPhase::Next((plan, cut)))
    }

    /// Replaces `request` with the shallowest summary depth whose paid input fits.
    async fn fit_harness_summary_depth(
        &self,
        runtime: &ExactCompactionRuntime,
        workflow_id: crate::SessionWorkflowId,
        cache_affinity: &ModelCacheAffinity,
        cut: &SemanticCompactionCut,
        request: &mut ModelRequest,
    ) -> Result<(), AgentSessionEndpointError> {
        // Measure each candidate with its own offline attempt identity.
        // Only the selected request is prepared with the provider attempt
        // below, persisted, and dispatched. A refusal during a fit probe
        // must never consume or reuse that dispatch capability.
        for depth in 0..=harness_compaction_reduction_count(cut) {
            let input = harness_compaction_input_at_depth(cut, workflow_id, depth)
                .map_err(|error| compact_runtime_endpoint_error(&error))?;
            let candidate = ModelRequest::from_parts(
                request.stable_prefix().clone(),
                input,
                request.metadata(),
            )
            .map_err(compact_runtime_endpoint_rejected)?;
            let probe = ModelRequest::from_parts(
                candidate.stable_prefix().clone(),
                candidate.input().clone(),
                RequestMetadata::for_session_workflow(
                    self.run_id,
                    workflow_id,
                    compaction_attempt_id(workflow_id, format!("summary-fit-{depth}").as_bytes())
                        .map_err(|error| compact_runtime_endpoint_error(&error))?,
                    candidate.metadata().created_at(),
                ),
            )
            .map_err(compact_runtime_endpoint_rejected)?;
            let measurement = self
                .measure_compaction_projection(
                    runtime,
                    probe,
                    CompactionFitBasis::PaidFoldInput,
                    Some(cache_affinity.clone()),
                    runtime.clock.now(),
                )
                .await;
            match measurement {
                Ok((status, _)) => {
                    *request = candidate;
                    if !matches!(
                        status.kind(),
                        ContextStatusKind::WouldOverflow(_)
                            | ContextStatusKind::ProtectedOverflow(_)
                    ) {
                        break;
                    }
                }
                // A lowering can refuse capacity without a ContextStatus,
                // including when its prepared record exceeds the size bound.
                // No dispatch exists, so try the next source-derived depth.
                Err(error) if error.model_kind() == Some(ModelErrorKind::ContextLimit) => {
                    *request = candidate;
                }
                // Unrelated preparation failures do not authorize further
                // elision. The unchanged preparation/fallback path below
                // reports the actual failure with no paid dispatch.
                Err(_) => {
                    *request = candidate;
                    break;
                }
            }
        }
        Ok(())
    }

    /// The free placeholder rung over this pass's measured source.
    pub(super) async fn compaction_source_placeholders(
        &mut self,
        attempt: &CompactionAttempt<'_>,
        runtime: &ExactCompactionRuntime,
        measured: &MeasuredSource,
        native_protected_history: bool,
    ) -> Result<AgentCompactSessionOutcome, AgentSessionEndpointError> {
        Box::pin(self.replace_protected_tool_results(
            attempt.active,
            attempt.origin,
            runtime,
            attempt.identity,
            &attempt.request.operation_id,
            attempt.source,
            measured.projection.identity(),
            measured.harness_plan,
            measured.before,
            measured.occupancy_before,
            native_protected_history,
        ))
        .await
    }

    /// Validates, authorizes and prepares the selected request. Preparation
    /// creates the workflow's sole dispatch capability; nothing is persisted yet.
    pub(super) async fn prepare_compaction_dispatch(
        &mut self,
        attempt: &CompactionAttempt<'_>,
        runtime: &ExactCompactionRuntime,
        measured: &MeasuredSource,
        plan: CompactionRequestPlan,
        committed_detail: &mut CompactionDetail,
    ) -> Result<CompactionPhase<PreparedCompaction>, AgentSessionEndpointError> {
        let CompactionRequestPlan {
            request: mut compaction_request,
            cut_identity,
            record_version,
        } = plan;
        let descriptor = runtime
            .provider_configuration
            .effective()
            .descriptor()
            .clone();
        let mut validated = descriptor
            .validate_request(compaction_request.clone(), ValidationMode::Strict)
            .map_err(ModelError::from)
            .map_err(|error| self.compact_preparation_endpoint_error(error))?;
        let (preparation, portable_preparation) = self
            .compaction_preparation_contexts(
                runtime,
                &descriptor,
                &validated,
                measured.cache_affinity.clone(),
                measured.native,
            )
            .await?;
        // Native compaction keeps the whole active head: its artifact preserves
        // exact replay and the anchored wire-item identities. A harness cut or
        // portable retry has different coverage and prepares without it. No
        // compaction attaches a previous-response continuation.
        let mut prepared =
            Arc::clone(&runtime.model).prepare_call(validated.clone(), preparation.clone());
        let native_protected_history = measured.native
            && prepared.as_ref().is_err_and(|error| {
                error.rejection_kind()
                    == Some(plakous_model::ModelPreparationErrorKind::ProtectedHistory)
            });
        if native_protected_history {
            let history = portable_compaction_history(compaction_request.input().history())
                .map_err(|error| compact_runtime_endpoint_error(&error))?;
            compaction_request = ModelRequest::from_parts(
                compaction_request.stable_prefix().clone(),
                ConversationView::from_shared(history, plakous_model::WorkingProjection::default()),
                compaction_request.metadata(),
            )
            .map_err(compact_runtime_endpoint_rejected)?;
            validated = descriptor
                .validate_request(compaction_request.clone(), ValidationMode::Strict)
                .map_err(ModelError::from)
                .map_err(|error| self.compact_preparation_endpoint_error(error))?;
            // The failed lowering created no dispatch capability. Retry fresh
            // once; only this selected call can ever be persisted and paid.
            prepared =
                Arc::clone(&runtime.model).prepare_call(validated.clone(), portable_preparation);
        }
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error)
                if measured.native
                    && error.rejection_kind()
                        == Some(plakous_model::ModelPreparationErrorKind::ArtifactReuse) =>
            {
                let error =
                    self.project_model_error(ModelError::from(error), ModelErrorPhase::Preparation);
                committed_detail.diagnostic = Some(format!(
                    "native facet artifact reuse refused: {}",
                    compact_diagnostic(&error)
                ));
                return Ok(CompactionPhase::RetryWithoutNative);
            }
            Err(error)
                if measured.native
                    && ModelError::from(error.clone()).kind() == ModelErrorKind::ContextLimit =>
            {
                let error =
                    self.project_model_error(ModelError::from(error), ModelErrorPhase::Preparation);
                committed_detail.diagnostic = Some(format!(
                    "native facet preparation refused for capacity: {}",
                    compact_diagnostic(&error)
                ));
                // Preparation created no capability or durable operation. Keep
                // this workflow's sole dispatch attempt and try its sealed cut.
                return Ok(CompactionPhase::RetryWithoutNative);
            }
            Err(error) => {
                return Ok(CompactionPhase::Done(
                    self.compaction_preparation_unavailable(
                        attempt,
                        runtime,
                        measured,
                        error,
                        native_protected_history,
                        committed_detail,
                    )
                    .await,
                ));
            }
        };
        Ok(CompactionPhase::Next(PreparedCompaction {
            request: compaction_request,
            cut_identity,
            record_version,
            validated,
            prepared,
            native_protected_history,
        }))
    }

    /// The preparation context for the selected request and its portable
    /// variant without the completed-history prefix.
    async fn compaction_preparation_contexts(
        &self,
        runtime: &ExactCompactionRuntime,
        descriptor: &plakous_model::ResolvedModelDescriptor,
        validated: &ValidatedRequestPlan,
        cache_affinity: ModelCacheAffinity,
        native: bool,
    ) -> Result<(ModelPreparationContext, ModelPreparationContext), AgentSessionEndpointError> {
        descriptor
            .preflight_context_admission_requirement(
                validated.request().context_admission_requirement(),
            )
            .map_err(ModelPreparationError::from)
            .map_err(ModelError::from)
            .map_err(|error| self.compact_preparation_endpoint_error(error))?;
        let prepared_at = runtime.clock.now();
        let authority = Arc::clone(&runtime.model)
            .revalidate_runtime_authority(prepared_at)
            .await
            .map_err(ModelPreparationError::from)
            .map_err(ModelError::from)
            .map_err(|error| self.compact_preparation_endpoint_error(error))?;
        validate_configuration_runtime_authority(
            &runtime.provider_configuration,
            descriptor,
            &authority,
            prepared_at,
        )
        .map_err(ModelError::from)
        .map_err(|error| self.compact_preparation_endpoint_error(error))?;
        // Paid dispatch uses the source branch's cache affinity; only the
        // successor checkpoint rotates to `target_affinity_entropy`.
        let preparation = ModelPreparationContext::new(authority, prepared_at)
            .with_required_input_bound()
            .with_request_anchor(self.request_anchor.clone())
            .with_cache_affinity(cache_affinity);
        let portable_preparation = preparation.clone();
        let mut preparation = self.with_completed_history_prefix(validated, preparation);
        if native {
            let (artifact, _) = self
                .active_model_preparation_state()
                .map_err(|error| compact_runtime_endpoint_error(&error))?;
            if let Some(artifact) = artifact {
                preparation = preparation.with_active_family_artifact(artifact);
            }
        }
        Ok((preparation, portable_preparation))
    }

    /// The compaction's own request could not be prepared. Nothing has been
    /// dispatched and nothing is owed, so this is not a terminal outcome for
    /// the ladder: it is one rung being unavailable. Falling to placeholders
    /// here is what stops the next round reaching the provider over the
    /// window. The live Grok growth case did exactly that — two compactions
    /// unavailable because an active Responses artifact no longer matched the
    /// request's canonical coverage, then a provider capacity denial, with no
    /// placeholder record anywhere.
    async fn compaction_preparation_unavailable(
        &mut self,
        attempt: &CompactionAttempt<'_>,
        runtime: &ExactCompactionRuntime,
        measured: &MeasuredSource,
        error: ModelPreparationError,
        native_protected_history: bool,
        committed_detail: &mut CompactionDetail,
    ) -> Result<AgentCompactSessionOutcome, AgentSessionEndpointError> {
        let error = self.project_model_error(ModelError::from(error), ModelErrorPhase::Preparation);
        let rung = if measured.native {
            "native facet"
        } else {
            "harness summary"
        };
        let preparation_refusal =
            format!("{rung} preparation failed: {}", compact_diagnostic(&error));
        committed_detail.diagnostic = Some(preparation_refusal.clone());
        self.compaction_source_placeholders(attempt, runtime, measured, native_protected_history)
            .await
            .and_then(|outcome| match outcome {
                // Placeholders could not act either; report the original
                // preparation failure rather than inventing a range
                // outcome for a rung that never got to run.
                AgentCompactSessionOutcome::NoEligibleRange => {
                    committed_detail.exhausted = true;
                    let guidance = if error.model_kind() != Some(ModelErrorKind::ContextLimit) {
                        ""
                    } else if attempt.origin.is_turn_fence() {
                        "; summary request exceeds capacity; no summary was dispatched and the source is unchanged; start a new session or fork with a handoff"
                    } else {
                        "; compaction cannot proceed within capacity; no summary was dispatched and the source is unchanged; when the session runs out of headroom, start a new session or fork with a handoff"
                    };
                    let reason = format!(
                        "{preparation_refusal}; no fitting offline successor (tool-result placeholders or replay omission){guidance}"
                    );
                    committed_detail.diagnostic = Some(reason.clone());
                    Err(compact_runtime_endpoint_rejected(reason))
                }
                outcome => Ok(outcome),
            })
    }

    pub(super) async fn prepare_compaction_operations(
        &mut self,
        identity: DurableCompactionIdentity,
        request_record: &DurableCompactionRequestRecord,
        prepared_record: &PreparedCallRecord,
    ) -> Result<(OperationId, OperationId), AgentError> {
        let compaction_operation = self.take_operation_id()?;
        let provider_operation = self.take_operation_id()?;
        let compaction_request = compaction_operation_component(request_record)?;
        let provider_request = compaction_operation_component(prepared_record)?;
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
        for record in [
            DurableOperationRecord::new(
                compaction_operation.0,
                DurableOperationKind::Compaction {
                    workflow_id: identity.workflow_id,
                },
                compaction_request,
                vec![DurableOperationTransition::new(
                    identity.prepared_checkpoint,
                    DurableOperationState::Prepared,
                )],
            ),
            DurableOperationRecord::new(
                provider_operation.0,
                DurableOperationKind::Provider {
                    root: ModelOperationRoot::SessionWorkflow(identity.workflow_id),
                    attempt_id: identity.attempt_id,
                },
                provider_request,
                vec![DurableOperationTransition::new(
                    identity.prepared_checkpoint,
                    DurableOperationState::Prepared,
                )],
            ),
        ] {
            ledger = ledger
                .append_operation(record.map_err(runtime_manifest_error)?)
                .map_err(runtime_manifest_error)?;
        }
        let lifecycle = working_lifecycle_for_root(
            ModelOperationRoot::SessionWorkflow(identity.workflow_id),
            &ledger,
        )?;
        self.publish_checkpoint(
            StoreAdmission::Ordinary,
            identity.prepared_checkpoint,
            lifecycle,
            history,
            ledger,
            context,
            family,
        )
        .await?;
        Ok((compaction_operation, provider_operation))
    }

    pub(super) async fn persist_compaction_locator(
        &mut self,
        identity: DurableCompactionIdentity,
        provider_operation: OperationId,
        checkpoint_id: CheckpointId,
        locator: &ProviderCallRecoveryLocator,
        expected_stage: ProviderCallRecoveryLocatorStage,
    ) -> Result<(), AgentError> {
        if locator.availability_stage() != expected_stage
            || locator.operation_root() != ModelOperationRoot::SessionWorkflow(identity.workflow_id)
        {
            return Err(AgentError::new(
                AgentErrorKind::Model,
                "provider recovery locator belongs to another compaction stage or root",
            ));
        }
        let (admission, history, ledger, context, family) = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            let provider = manifest
                .ledger()
                .operation(provider_operation.0)
                .ok_or_else(|| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "compaction provider operation disappeared",
                    )
                })?;
            let prepared: PreparedCallRecord = serde_json::from_slice(provider.request().bytes())
                .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Session,
                    "prepared compaction provider call is not reconstructive",
                )
            })?;
            prepared
                .recovery_request_with_locator(locator.clone())
                .map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::Model,
                        "provider recovery locator does not match the prepared compaction call",
                    )
                })?;
            let evidence = runtime_component(
                RuntimeComponentKind::Operation,
                serde_json::to_vec(locator).map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "provider recovery locator could not be encoded",
                    )
                })?,
            )?;
            let target = DurableOperationState::LocatorBound(evidence);
            (
                transition_admission(provider.state(), &target),
                manifest.canonical().history().clone(),
                manifest
                    .ledger()
                    .transition_operation(
                        provider_operation.0,
                        DurableOperationTransition::new(checkpoint_id, target),
                    )
                    .map_err(runtime_manifest_error)?,
                manifest.canonical().context().retained_for_exact_resume(),
                manifest.family_artifact().cloned(),
            )
        };
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
