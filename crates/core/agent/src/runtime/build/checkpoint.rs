//! Checkpoint authority, manifest binding, genesis checkpoint and startup restore before publication.

#[cfg(test)]
mod tests;

use super::{
    AgentBuilder, AgentError, AgentErrorKind, Arc, BindingReference, BranchId, CanonicalHistory,
    CheckpointId, CommandId, CompiledProviderConfiguration, ContinuationToolSlot, Digest,
    DigestDomain, DurableRuntimeBootstrap, FrozenToolExposureDescriptor,
    FrozenToolProjectionDescriptorV2, HarnessPlanComponentEnvelope, InterpreterAvailability,
    InterpreterObservation, InterpreterResetReason, LanguageModel, LocalCompositionConstruction,
    LocalCompositionLifecycle, ModelRouteBinding, PreparedCheckpoint, ProducerProvenance,
    ReconstructiveComponent, ResolvedHarnessPlan, RestoredContinuationFrame,
    RestoredEffectDecision, RestoredStartup, RunId, RuntimeCanonicalState, RuntimeCheckpoint,
    RuntimeCheckpointCodec, RuntimeComponentKind, RuntimeContextState, RuntimeDurability,
    RuntimeLifecycle, RuntimeManifest, RuntimeOperationLedger, RuntimeRehydration, RuntimeTopology,
    SessionId, SessionStoreError, StableRequestPrefix, TOOL_EXPOSURE_DESCRIPTOR_CODEC_REVISION,
    TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION, TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION,
    TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION, Timestamp, ToolInvocationRequest, ToolPresentation,
    ToolRuntime, ToolRuntimeManifest, WorkingProjection, WorkspacePolicy, checkpoint_bundle_error,
    control_agent_error, live_job_config, queued_turn_machine, refreshed_reopen_admission,
    runtime_component, runtime_component_revision, runtime_manifest_error,
    runtime_rehydration_error, runtime_revalidation_witness, stable_runtime_environment_component,
    tool_descriptor_error,
};

/// Admits a reopened checkpoint as the authority after checking that this
/// build reproduces its binding, request prefix and harness plan.
pub(super) fn reopened_checkpoint_authority(
    builder: &AgentBuilder,
    bootstrap: DurableRuntimeBootstrap,
    stable_request_prefix: &StableRequestPrefix,
    harness_plan: &ResolvedHarnessPlan,
    tools: &ToolRuntime,
    tool_presentation: &ToolPresentation,
    restores_working: bool,
) -> Result<PreparedCheckpoint, AgentError> {
    let manifest = bootstrap.checkpoint.manifest();
    // A different model is the caller's binding failing revalidation;
    // the same model with a different prefix is this build drifting.
    if manifest.binding().model_binding()
        != builder
            .provider_configuration
            .effective()
            .descriptor()
            .semantic_binding_digest()
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::InvalidInput,
                    "resolved model binding could not be validated",
                )
            })?
    {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "builder model binding disagrees with the durable checkpoint",
        ));
    }
    let durable_prefix = manifest.canonical().stable_prefix();
    if durable_prefix != stable_request_prefix {
        return Err(AgentError::new(
            AgentErrorKind::IncompatibleSession,
            checkpoint_prefix_mismatch(stable_request_prefix.drift_from(durable_prefix)),
        ));
    }
    let witness = runtime_revalidation_witness(
        builder.model.as_ref(),
        &builder.provider_configuration,
        tools,
        tool_presentation,
        builder.workspace.as_ref(),
        builder.local_composition.as_ref(),
        refreshed_reopen_admission(
            manifest,
            &builder.provider_configuration,
            &bootstrap.admission,
        )?,
        builder.clock.now(),
    )?;
    let restored_plan = match bootstrap
        .checkpoint
        .rehydrate(&witness)
        .map_err(runtime_rehydration_error)?
    {
        RuntimeRehydration::Quiescent { harness_plan, .. } => harness_plan,
        RuntimeRehydration::WorkingRecovery { harness_plan, .. } if restores_working => {
            harness_plan
        }
        RuntimeRehydration::WorkingRecovery { .. } => {
            return Err(AgentError::new(
                AgentErrorKind::OutcomeUnknown,
                "working checkpoint requires the explicit recovery workflow",
            ));
        }
    };
    if &restored_plan != harness_plan {
        return Err(AgentError::new(
            AgentErrorKind::IncompatibleSession,
            "builder harness plan disagrees with durable reconstruction",
        ));
    }
    Ok(PreparedCheckpoint::new(
        bootstrap.checkpoint,
        bootstrap.bundle,
        Some(bootstrap.branch),
    ))
}

/// Builds the in-memory initial checkpoint of a fresh session.
pub(super) fn fresh_checkpoint_authority(
    builder: &AgentBuilder,
    (session_id, branch_id, run_id): (SessionId, BranchId, RunId),
    stable_request_prefix: &StableRequestPrefix,
    harness_plan: &ResolvedHarnessPlan,
    tools: &ToolRuntime,
    tool_presentation: &ToolPresentation,
) -> Result<PreparedCheckpoint, AgentError> {
    let checkpoint_id = CheckpointId::try_from(run_id.as_uuid())
        .map_err(|error| AgentError::from_identity(&error))?;
    let checkpoint = initial_memory_checkpoint(
        session_id,
        branch_id,
        checkpoint_id,
        stable_request_prefix,
        harness_plan,
        &builder.provider_configuration,
        builder.model.as_ref(),
        tools,
        tool_presentation,
        builder.workspace.as_ref(),
        builder.local_composition.as_ref(),
        builder.clock.now(),
        None,
        RuntimeDurability::Disabled,
        CanonicalHistory::default(),
        builder.initial_named_state.clone(),
        builder.job_counter_seed,
    )?;
    let bundle =
        RuntimeCheckpointCodec::bundle(&checkpoint, None).map_err(checkpoint_bundle_error)?;
    Ok(PreparedCheckpoint::new(Arc::new(checkpoint), bundle, None))
}

/// A persistent interpreter restarts on every reopen: the next generation is
/// recorded as reset by the process reopen.
pub(super) fn reopened_interpreter_observation(
    builder: &AgentBuilder,
    authority: &PreparedCheckpoint,
) -> Result<Option<InterpreterObservation>, AgentError> {
    let observation = if builder.local_composition.as_ref().is_some_and(|value| {
        value.integration.lifecycle() == LocalCompositionLifecycle::AgentIncarnationPersistent
    }) {
        let generation = authority
            .checkpoint()
            .manifest()
            .interpreter_observation()
            .map(InterpreterObservation::generation)
            .map_or(Ok(1), |generation| {
                generation.checked_add(1).ok_or_else(|| {
                    AgentError::new(
                        AgentErrorKind::Session,
                        "interpreter generation identity space was exhausted on reopen",
                    )
                })
            })?;
        Some(
            InterpreterObservation::new(
                generation,
                InterpreterAvailability::Reset,
                Some(InterpreterResetReason::ProcessReopen),
                authority
                    .checkpoint()
                    .manifest()
                    .interpreter_observation()
                    .and_then(InterpreterObservation::affected_cell)
                    .map(str::to_owned),
                None,
            )
            .map_err(runtime_manifest_error)?,
        )
    } else {
        None
    };
    Ok(observation)
}

/// Re-arms a restored continuation frame, including a still-pending effect
/// decision.
pub(super) fn restore_startup(
    builder: &AgentBuilder,
    frame: RestoredContinuationFrame,
    run_id: RunId,
    tools: &ToolRuntime,
    tool_presentation: &ToolPresentation,
) -> Result<RestoredStartup, AgentError> {
    let command_id =
        CommandId::new(frame.root().command_id.clone()).map_err(control_agent_error)?;
    let lifecycle = queued_turn_machine(frame.root().turn_id)?;
    let effect = if let RestoredContinuationFrame::EffectDecision(effect_frame) = &frame {
        if builder.clock.now() >= effect_frame.decision_expires_at {
            return Err(AgentError::new(
                AgentErrorKind::OutcomeUnknown,
                "persisted effect decision expired",
            ));
        }
        let pending_index = usize::try_from(effect_frame.pending_index)
            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
        let Some(ContinuationToolSlot::PendingDecision { call, .. }) =
            effect_frame.slots.get(pending_index)
        else {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        };
        let arguments = tool_presentation
            .registry_arguments(call)
            .map_err(|error| AgentError::from_tool(&error))?;
        let prepared = tool_presentation
            .prepare(
                ToolInvocationRequest::new(
                    run_id,
                    effect_frame.root.turn_id,
                    effect_frame.attempt_id,
                    call.call_id(),
                    call.exposed_name().clone(),
                    call.correlation().clone(),
                    Arc::<str>::from(arguments),
                ),
                tools.environment(),
            )
            .map_err(|error| AgentError::from_tool(&error))?;
        let pending = tools
            .restore_pending_effect_decision(
                prepared.invocation.clone(),
                effect_frame.decision.binding_digest(),
                plakous_tools::EffectDecisionPrompt::new(&effect_frame.decision_prompt),
            )
            .map_err(|error| AgentError::from_tool(&error))?;
        Some(RestoredEffectDecision {
            invocation: prepared.invocation,
            projection_receipt: prepared.projection_receipt,
            pending,
        })
    } else {
        None
    };
    Ok(RestoredStartup {
        frame,
        effect,
        command_id,
        lifecycle,
    })
}

pub(in crate::runtime) fn harness_plan_component(
    harness_plan: &ResolvedHarnessPlan,
) -> Result<ReconstructiveComponent, AgentError> {
    let bytes = HarnessPlanComponentEnvelope::capture(
        harness_plan.source_profile(),
        harness_plan.source_inputs(),
        harness_plan,
        crate::RUNTIME_POLICY_REVISION,
    )
    .and_then(|envelope| envelope.encode())
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "resolved harness plan is not reconstruction-complete",
        )
    })?;
    runtime_component(RuntimeComponentKind::HarnessPlan, bytes)
}

#[allow(
    clippy::too_many_lines,
    reason = "Manifest binding reconstruction resolves the frozen registry and presentation against the validated runtime binding"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "the checkpoint binding consumes the complete live reconstruction authority tuple"
)]
pub(in crate::runtime) fn runtime_manifest_binding_and_tools(
    _model: &dyn LanguageModel,
    provider_configuration: &CompiledProviderConfiguration,
    tools: &ToolRuntime,
    presentation: &ToolPresentation,
    workspace: &WorkspacePolicy,
    local_composition: Option<&LocalCompositionConstruction>,
    observed_at: Timestamp,
    fresh_admission: Option<crate::RevalidationFact>,
) -> Result<(ModelRouteBinding, ToolRuntimeManifest), AgentError> {
    let descriptor = provider_configuration.effective().descriptor();
    let descriptor_bytes = serde_json::to_vec(descriptor).map_err(|_| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "resolved model descriptor could not be captured",
        )
    })?;
    let descriptor_component = runtime_component(
        RuntimeComponentKind::ModelDescriptor,
        descriptor_bytes.clone(),
    )?;
    let account = fresh_admission
        .as_ref()
        .map(|admission| admission.reference().clone())
        .unwrap_or(
            BindingReference::new("non-durable-model", descriptor.route().as_str(), 1)
                .map_err(runtime_manifest_error)?,
        );
    let profile = BindingReference::new("model-adapter", descriptor.profile().as_str(), 1)
        .map_err(runtime_manifest_error)?;
    let admission_evidence =
        runtime_component(RuntimeComponentKind::AdmissionEvidence, descriptor_bytes)?;
    let admission = match fresh_admission {
        Some(admission) => admission,
        None => {
            crate::RevalidationFact::new(account.clone(), observed_at, None, admission_evidence)
                .map_err(runtime_manifest_error)?
        }
    };
    let binding = ModelRouteBinding::new(
        descriptor.protocol().clone(),
        descriptor.route().clone(),
        descriptor.requested_model().clone(),
        descriptor.reported_model().cloned(),
        descriptor.model_family().cloned(),
        descriptor.profile().clone(),
        account,
        profile,
        descriptor.semantic_binding_digest().map_err(|_| {
            AgentError::new(
                AgentErrorKind::InvalidInput,
                "resolved model binding could not be captured",
            )
        })?,
        descriptor_component,
        admission,
    )
    .map_err(runtime_manifest_error)?;

    let registry_descriptor = tools.registry_descriptor().map_err(tool_descriptor_error)?;
    let (registry_revision, registry_bytes, exposure_revision, exposure_bytes) = match presentation
    {
        ToolPresentation::V1(exposure) => (
            TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION,
            registry_descriptor
                .encode_v1()
                .map_err(tool_descriptor_error)?,
            TOOL_EXPOSURE_DESCRIPTOR_CODEC_REVISION,
            FrozenToolExposureDescriptor::capture(exposure)
                .and_then(|descriptor| descriptor.encode_v1())
                .map_err(tool_descriptor_error)?,
        ),
        ToolPresentation::V2(projection) => (
            TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION,
            registry_descriptor
                .encode_v2()
                .map_err(tool_descriptor_error)?,
            TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION,
            FrozenToolProjectionDescriptorV2::capture(projection)
                .and_then(|descriptor| descriptor.encode_v2())
                .map_err(tool_descriptor_error)?,
        ),
    };
    let registry = runtime_component_revision(
        RuntimeComponentKind::ToolRegistry,
        registry_revision,
        registry_bytes,
    )?;
    let exposure_component = runtime_component_revision(
        RuntimeComponentKind::ToolExposure,
        exposure_revision,
        exposure_bytes,
    )?;
    let environment = stable_runtime_environment_component(
        workspace,
        tools.environment().descriptor().backend(),
        local_composition,
        provider_configuration,
        live_job_config(tools)?,
    )?;
    let policy_revision = tools.policy_revision();
    let effect_policy = runtime_component(
        RuntimeComponentKind::EffectPolicy,
        format!(
            "{}:{}",
            policy_revision.policy_id(),
            policy_revision.revision()
        )
        .into_bytes(),
    )?;
    let tool_manifest = ToolRuntimeManifest::new(
        registry,
        exposure_component,
        environment,
        effect_policy,
        BindingReference::new(
            "tool-policy",
            policy_revision.policy_id(),
            policy_revision.revision(),
        )
        .map_err(runtime_manifest_error)?,
    )
    .map_err(runtime_manifest_error)?;
    Ok((binding, tool_manifest))
}

#[allow(
    clippy::too_many_arguments,
    reason = "genesis closes the complete manifest groups in one auditable constructor"
)]
pub(in crate::runtime) fn initial_memory_checkpoint(
    session_id: SessionId,
    branch_id: BranchId,
    checkpoint_id: CheckpointId,
    stable_prefix: &StableRequestPrefix,
    harness_plan: &ResolvedHarnessPlan,
    provider_configuration: &CompiledProviderConfiguration,
    model: &dyn LanguageModel,
    tools: &ToolRuntime,
    presentation: &ToolPresentation,
    workspace: &WorkspacePolicy,
    local_composition: Option<&LocalCompositionConstruction>,
    observed_at: Timestamp,
    fresh_admission: Option<crate::RevalidationFact>,
    durability: RuntimeDurability,
    history: CanonicalHistory,
    named_state: crate::manifest::NamedState,
    job_counter_seed: u32,
) -> Result<RuntimeCheckpoint, AgentError> {
    let (binding, tool_manifest) = runtime_manifest_binding_and_tools(
        model,
        provider_configuration,
        tools,
        presentation,
        workspace,
        local_composition,
        observed_at,
        fresh_admission,
    )?;
    let harness = harness_plan_component(harness_plan)?;
    let mut context_entropy = [0_u8; 32];
    getrandom::fill(&mut context_entropy).map_err(|_| {
        AgentError::new(
            AgentErrorKind::Identity,
            "runtime context identity randomness is unavailable",
        )
    })?;
    let context = RuntimeContextState::genesis(session_id, branch_id, context_entropy)
        .map_err(runtime_manifest_error)?;
    let canonical = RuntimeCanonicalState::new(
        stable_prefix.clone(),
        history,
        WorkingProjection::default(),
        context,
    )
    .map_err(runtime_manifest_error)?;
    let commit_identity = Digest::sha256(
        DigestDomain::ProviderOperation,
        2,
        &[
            b"plakous-agent/genesis-checkpoint-v1",
            session_id.as_uuid().as_bytes(),
            branch_id.as_uuid().as_bytes(),
            checkpoint_id.as_uuid().as_bytes(),
        ],
    )
    .map_err(|_| AgentError::new(AgentErrorKind::Internal, "genesis identity failed"))?;
    let manifest = RuntimeManifest::new(
        ProducerProvenance::new("plakous-agent", env!("CARGO_PKG_VERSION"), "runtime")
            .map_err(runtime_manifest_error)?,
        RuntimeTopology {
            session_id,
            branch_id,
            checkpoint_id,
            parent_checkpoint_id: None,
            branch_revision: 0,
            fork: None,
            lease_fence: 1,
            commit_identity,
        },
        RuntimeLifecycle::Quiescent,
        harness,
        provider_configuration.clone(),
        canonical,
        binding,
        tool_manifest,
        RuntimeOperationLedger::new(Vec::new(), Vec::new()).map_err(runtime_manifest_error)?,
        None,
        None,
        crate::manifest::JobLedger::new(job_counter_seed, Vec::new())
            .map_err(runtime_manifest_error)?,
        named_state,
        durability,
    )
    .map_err(runtime_manifest_error)?;
    RuntimeCheckpoint::new(manifest).map_err(runtime_manifest_error)
}

/// The reopen's stable-prefix mismatch, naming the differing fields when any
/// field differs; the facade passes only closed field names through.
fn checkpoint_prefix_mismatch(drift: plakous_model::StableRequestPrefixDrift) -> String {
    const MISMATCH: &str = "builder semantics disagree with the durable checkpoint";
    if drift.is_empty() {
        MISMATCH.to_owned()
    } else {
        format!("{MISMATCH} (stable-prefix fields: {drift})")
    }
}
