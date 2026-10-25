//! Prepare exact runtime configuration and owned resources before actor publication.

mod checkpoint;
mod environment;
mod plan;
mod presentation;

pub(super) use plan::{
    resolve_construction_harness_plan, stable_prefix_from_effective_configuration,
};

use checkpoint::{
    fresh_checkpoint_authority, reopened_checkpoint_authority, reopened_interpreter_observation,
    restore_startup,
};
pub(super) use checkpoint::{
    harness_plan_component, initial_memory_checkpoint, runtime_manifest_binding_and_tools,
};

pub use environment::compile_explicit_runtime_profile;
use environment::stable_workspace_environment_digest;
pub(super) use environment::{
    live_job_config, stable_runtime_environment_component, stable_workspace_environment_bytes,
};
pub(crate) use environment::{stored_composition_artifact, stored_job_config};

#[cfg(test)]
pub(super) use presentation::local_composition_binding_identity;

pub(super) use presentation::{
    construction_tool_presentation, tool_registry_descriptor,
    validate_local_composition_attachment_identity, validate_local_composition_binding_slot,
    validate_reopened_projection,
};

use crate::local_composition::{LocalCompositionConstruction, LocalCompositionLifecycle};
use crate::manifest::{
    InterpreterAvailability, InterpreterObservation, InterpreterResetReason, RuntimeContextState,
};
use crate::profile::compile_construction_bound_plan;
use crate::runtime::RequestSlots;
use crate::runtime::admission::{
    refreshed_reopen_admission, validate_configuration_runtime_authority,
};
use crate::runtime::checkpoint::io::{
    DurabilityCommitTelemetry, StoreCallError, StorePhase, store_call,
};
use crate::runtime::checkpoint::owner::PreparedCheckpoint;
use crate::runtime::config::{AgentConfig, AgentRequestPolicy};
use crate::runtime::continuation::{
    ContinuationToolSlot, DurableAgentConfig, RestoredContinuationFrame, checkpoint_bundle_error,
    decode_restored_continuation, profile_construction_error, reconstruct_persisted_tools,
    rehydrate_runtime_indexes, runtime_component, runtime_component_revision,
    runtime_manifest_error, runtime_rehydration_error, runtime_revalidation_witness,
    tool_descriptor_error,
};
use crate::runtime::control::{
    control_agent_error, event_error, run_environment, subscription_limits,
};
use crate::runtime::durable::turn_recovery::reopen_closes_working_turn;
use crate::runtime::durable::workflow_closure::WorkflowClosureRuntime;
use crate::runtime::terminal::queued_turn_machine;
use crate::runtime::{
    Agent, AgentBuilder, DurableRuntimeBootstrap, PostCompactionNote, PreparedAgent,
    PreparedToolInvocation, RestoredEffectDecision, RestoredStartup, ToolPresentation,
};
use crate::tool_runtime::{ToolRuntime, ToolRuntimeConfig, ToolScheduler};
use crate::{
    AgentError, AgentErrorKind, BindingReference, CheckpointId, CommandId, EventHub,
    EventHubLimits, HarnessPlanComponentEnvelope, HarnessProfileSpec, ModelRouteBinding,
    ProducerProvenance, ProfileError, ReconstructiveComponent, ResolvedContextSourceFact,
    ResolvedHarnessPlan, RuntimeCanonicalState, RuntimeCheckpoint, RuntimeCheckpointCodec,
    RuntimeComponentKind, RuntimeDurability, RuntimeLifecycle, RuntimeManifest,
    RuntimeOperationLedger, RuntimeRehydration, RuntimeTopology, SessionStoreError,
    TelemetryContext, TelemetryEmitter, ToolRuntimeManifest, ViewError,
    compiler_model_facts_from_model,
};
use plakous_model::{
    AgentId, BranchId, CachePolicy, CanonicalHistory, CapabilityRequirement, Clock,
    CompiledProviderConfiguration, ConfiguredLanguageModel, Digest, DigestDomain,
    EffectiveProviderConfigurationId, EventId, GenerationLimits, IdGenerator, InstructionBlock,
    InstructionSource, LanguageModel, LocalDurabilityDisposition, ModelErrorTarget, ModelId,
    NativeOptions, ProjectedToolPresentationV2, ReasoningEffortDisposition, ReasoningPolicy, RunId,
    SelectedProviderConfigurationId, SessionId, StableRequestPrefix, SystemClock,
    SystemIdGenerator, Timestamp, ToolCallBlock, ToolEffect, ToolExecutionDisposition,
    ToolInputContract, ToolName, WorkingProjection,
};
use plakous_tools::{
    ExecutionEnvironment, FrozenToolExposureDescriptor, FrozenToolProjectionDescriptorV2,
    FrozenToolRegistry, FrozenToolRegistryDescriptor, ProjectionCandidateAdmission,
    ReadOnlyToolPolicy, TOOL_EXPOSURE_DESCRIPTOR_CODEC_REVISION,
    TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION, TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION,
    TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION, ToolExecutorBindings, ToolExposure,
    ToolExposureMap, ToolInvocationRequest, ToolOutcome, ToolPolicy, ToolPolicyRevision,
    ToolProjectionMap, ToolProjectionReceiptV2, ToolRuntimeError, ToolRuntimeErrorKind,
    WorkspacePolicy, validate_custom_process_presentation_v2,
};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

pub(super) fn caller_instructions_match(
    left: &[InstructionBlock],
    right: &[InstructionBlock],
) -> bool {
    let caller_owned = |block: &&InstructionBlock| {
        matches!(
            block.source(),
            InstructionSource::System | InstructionSource::Developer
        )
    };
    left.iter()
        .filter(caller_owned)
        .eq(right.iter().filter(caller_owned))
}

impl fmt::Debug for AgentBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentBuilder")
            .field(
                "model",
                self.provider_configuration.effective().descriptor(),
            )
            .field(
                "initial_model_authority",
                &self
                    .initial_model_authority
                    .execution_authority_binding()
                    .ok(),
            )
            .field(
                "selected_provider_configuration",
                &self.provider_configuration.selected_identity(),
            )
            .field(
                "effective_provider_configuration",
                &self.provider_configuration.effective_identity(),
            )
            .field("tool_count", &self.tools.len())
            .field(
                "tool_projection",
                &self.tool_projection.as_ref().map(ToolProjectionMap::digest),
            )
            .field("workspace", &"<redacted>")
            .field("config", &self.config)
            .field("request_policy", &self.request_policy)
            .field("instruction_count", &self.instructions.len())
            .field(
                "harness_plan_digest",
                &self
                    .harness_plan
                    .as_ref()
                    .map(ResolvedHarnessPlan::plan_digest),
            )
            .field("tool_policy", &self.policy.revision())
            .finish_non_exhaustive()
    }
}

impl AgentBuilder {
    pub(crate) fn reachability_probe(&self) -> Option<plakous_model::ReachabilityProbe> {
        self.model.reachability_probe()
    }

    pub(crate) fn session_input_window(&self) -> Option<(Option<u64>, u64)> {
        use plakous_model::CapacityRule;
        let rule = self
            .provider_configuration
            .effective()
            .descriptor()
            .context_contract()?
            .capacity()
            .rule();
        match rule {
            CapacityRule::ProviderReservedInput {
                reported_context_window,
                maximum_projected_input,
                ..
            } => Some((reported_context_window, maximum_projected_input)),
            CapacityRule::SharedWindow { total }
            | CapacityRule::SharedWindowWithOutputCap { total, .. } => {
                let reserved = u64::from(
                    self.request_policy
                        .generation_limits()
                        .max_output_tokens()?
                        .get(),
                );
                Some((
                    Some(total.lower_bound()),
                    total.lower_bound().saturating_sub(reserved),
                ))
            }
            CapacityRule::Separate { input, .. } => Some((None, input.lower_bound())),
            _ => None,
        }
    }

    pub(crate) fn session_target_authority(
        &self,
    ) -> (
        plakous_model::ProtocolId,
        plakous_model::RouteId,
        ModelId,
        SelectedProviderConfigurationId,
        EffectiveProviderConfigurationId,
        Digest,
    ) {
        let descriptor = self.provider_configuration.effective().descriptor();
        (
            descriptor.protocol().clone(),
            descriptor.route().clone(),
            descriptor.requested_model().clone(),
            self.provider_configuration.selected_identity(),
            self.provider_configuration.effective_identity(),
            self.initial_model_authority.account_binding(),
        )
    }

    /// Creates a builder from an exact resolved model, frozen tools, and workspace policy.
    #[must_use]
    #[allow(
        clippy::needless_pass_by_value,
        reason = "construction consumes the sealed model/configuration/authority aggregate as one capability"
    )]
    pub fn new(
        configured_model: ConfiguredLanguageModel,
        tools: FrozenToolRegistry,
        workspace: Arc<WorkspacePolicy>,
    ) -> Self {
        let model = configured_model.model();
        let provider_configuration = Arc::new(configured_model.configuration().clone());
        let initial_model_authority = configured_model.initial_authority().clone();
        Self {
            model,
            provider_configuration,
            initial_model_authority,
            tool_bindings: ToolExecutorBindings::capture(&tools),
            tools,
            tool_projection: None,
            projection_admission: None,
            workspace,
            host_process_options: plakous_tools::HostProcessOptions::default(),
            job_config: plakous_tools::JobConfig::default(),
            job_counter_seed: 1,
            job_host: None,
            config: AgentConfig::default(),
            request_policy: AgentRequestPolicy::default(),
            instructions: Vec::new(),
            harness_plan: None,
            clock: Arc::new(SystemClock),
            ids: Arc::new(SystemIdGenerator),
            event_ids: Arc::new(SystemIdGenerator),
            policy: Arc::new(ReadOnlyToolPolicy),
            telemetry: Arc::new(TelemetryEmitter::disabled()),
            tool_scheduler: None,
            local_composition: None,
            post_compaction_note: None,
            model_reresolver: None,
            request_slots: None,
        }
    }

    /// Binds process foreground and hard-cap limits at construction or rebind.
    #[must_use]
    pub fn job_config(mut self, config: plakous_tools::JobConfig) -> Self {
        self.job_config = config;
        self
    }

    /// Seeds a cleared runtime with its source's next never-used process number.
    ///
    /// # Errors
    /// Rejects the reserved zero number. Exact reopen always uses its stored counter.
    pub fn job_counter_seed(mut self, next_id: u32) -> Result<Self, AgentError> {
        if next_id == 0 {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "job counter seed must be nonzero",
            ));
        }
        self.job_counter_seed = next_id;
        Ok(self)
    }

    /// Uses a host-owned process registry instead of a private service.
    #[must_use]
    pub fn job_host(mut self, host: Arc<dyn plakous_tools::JobHost>) -> Self {
        self.job_host = Some(host);
        self
    }

    /// Supplies fresh host process protection and a required workspace PATH prefix.
    /// The prefix policy is revalidated during construction and durable reopen.
    #[must_use]
    pub fn host_process_options(mut self, options: plakous_tools::HostProcessOptions) -> Self {
        self.host_process_options = options;
        self
    }

    /// Enables one exact driver-mediated local programmatic-composition profile.
    ///
    /// The descriptor owns the exact outer and seven-tool nested projections;
    /// the integration has already passed its exact release admission.
    /// Construction revalidates that the descriptor owns this builder's frozen
    /// registry and replaces the provider-visible projection with its exact
    /// `exec`/`wait` surface.
    ///
    /// # Errors
    ///
    /// Fails when the descriptor disagrees with the exact frozen registry or target.
    #[doc(hidden)]
    pub fn local_programmatic_composition(
        mut self,
        construction: LocalCompositionConstruction,
    ) -> Result<Self, AgentError> {
        validate_local_composition_binding_slot(self.local_composition.is_some())?;
        let descriptor = &construction.descriptor;
        let registry_identity = self
            .tools
            .registry_identity_digest()
            .map_err(|error| AgentError::from_tool(&error))?;
        if descriptor.outer_projection.registry_digest() != registry_identity
            || descriptor.nested_projection.registry_digest() != registry_identity
            || descriptor.outer_projection.target() != descriptor.nested_projection.target()
        {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "local composition descriptor does not own this builder's exact registry and target",
            ));
        }
        validate_local_composition_attachment_identity(&construction.attachment)?;
        self.tool_projection = Some(descriptor.outer_projection.clone());
        self.local_composition = Some(construction);
        Ok(self)
    }

    /// Replaces the fresh process-local executor table used for durable reopen.
    ///
    /// Persisted definitions remain semantic authority. Extra bindings are
    /// unreachable, and a missing exact executor ID fails before dispatch.
    #[must_use]
    pub fn tool_executor_bindings(mut self, bindings: ToolExecutorBindings) -> Self {
        self.tool_bindings = bindings;
        self
    }

    /// Selects one exact prequalified V2 model projection for this agent.
    ///
    /// Construction fails closed unless the map owns the same frozen registry
    /// snapshot supplied to [`Self::new`]. Provider/model qualification is
    /// retained exactly by the map; the agent performs no projection inference.
    #[must_use]
    pub fn tool_projection(mut self, projection: ToolProjectionMap) -> Self {
        self.tool_projection = Some(projection);
        self
    }

    /// Supplies the authority that admits this agent's V2 projection as the
    /// exact candidate for its provider configuration.
    ///
    /// Whether a projection is the qualified candidate depends on the pack list
    /// it was compiled from and the route catalog that chose the vocabulary —
    /// both facts of the composition root, not of the agent. A standard
    /// integration that presents a V2 projection without this port is refused.
    #[must_use]
    pub fn projection_admission(
        mut self,
        admission: Arc<dyn ProjectionCandidateAdmission>,
    ) -> Self {
        self.projection_admission = Some(admission);
        self
    }

    /// Replaces the process-wide default tool scheduler with an explicit instance.
    ///
    /// Use for multi-session embeddings that must not share the global 64-slot
    /// default scheduler budget. Single-agent plak leaves this unset.
    #[must_use]
    pub fn tool_scheduler(mut self, scheduler: ToolScheduler, config: ToolRuntimeConfig) -> Self {
        self.tool_scheduler = Some((scheduler, config));
        self
    }
    /// Replaces runtime limits.
    #[must_use]
    pub const fn config(mut self, config: AgentConfig) -> Self {
        self.config = config;
        self
    }

    /// Enables or disables scheduled compact on the built agent.
    #[must_use]
    pub const fn with_scheduled_compaction_enabled(mut self, enabled: bool) -> Self {
        self.config.scheduled_compaction_enabled = enabled;
        self
    }

    /// Replaces the immutable provider-neutral model-request policy.
    #[must_use]
    pub fn request_policy(mut self, request_policy: AgentRequestPolicy) -> Self {
        self.request_policy = request_policy;
        self
    }

    /// Appends the note `note` derives from a committed compaction's source
    /// and successor, when it derives one.
    ///
    /// A note is a steering segment: during a turn it is appended right after
    /// the commit, and after an idle compaction it opens the next turn. The
    /// runtime knows nothing of what it says; a tool pack supplies it (the plan
    /// pack restores the latest task list this way). A native compaction
    /// successor drops earlier harness notes ahead of its compaction item, so
    /// `note` must not rely on seeing its own earlier note there.
    #[must_use]
    pub fn post_compaction_note(mut self, note: PostCompactionNote) -> Self {
        self.post_compaction_note = Some(note);
        self
    }

    /// Shares `slots` as this agent's bound on concurrent model requests.
    ///
    /// Each model request, compaction summaries included, waits in arrival
    /// order for a slot before its operation deadline starts and holds it
    /// until its response stream ends. A wait publishes
    /// [`crate::AgentEventKind::ModelRequestQueued`]; cancellation ends it.
    #[must_use]
    pub fn request_slots(mut self, slots: RequestSlots) -> Self {
        self.request_slots = Some(slots);
        self
    }

    /// Lets the runtime continue past the admitted evidence window of the
    /// resolved model, which ends at `admitted_until`.
    ///
    /// Between model operations past that instant, and never with a prepared
    /// call outstanding, the runtime asks `reresolver` for a fresh exact
    /// resolution and swaps it in only when the selected and effective
    /// configuration identities, account and credential authority are
    /// unchanged and the credential generation has not decreased. Canonical
    /// history and the durable binding are untouched. Without a reresolver a
    /// model past its window fails as its own revalidation reports.
    #[must_use]
    pub fn model_reresolver(
        mut self,
        reresolver: Arc<dyn super::ModelReresolver>,
        admitted_until: Timestamp,
    ) -> Self {
        self.model_reresolver = Some((reresolver, admitted_until));
        self
    }

    /// Narrows generation limits to the harness compaction bound for this
    /// builder's own output bound and reasoning, preserving everything else.
    ///
    /// # Errors
    ///
    /// Returns the request error when the compaction limits are invalid.
    pub fn with_compaction_generation_limits(
        mut self,
    ) -> Result<Self, plakous_model::RequestError> {
        let limits = self.request_policy.generation_limits();
        let bound = plakous_model::semantic_compaction_output_bound(
            limits.max_output_tokens(),
            self.request_policy.reasoning(),
        );
        self.request_policy = self
            .request_policy
            .with_generation_limits(GenerationLimits::new(Some(bound), Vec::new())?);
        Ok(self)
    }

    /// Replaces the ordered canonical instruction prefix.
    #[must_use]
    pub fn instructions(mut self, instructions: Vec<InstructionBlock>) -> Self {
        self.instructions = instructions;
        self
    }

    /// Binds an explicit immutable resolved harness plan.
    ///
    /// When supplied, construction revalidates the plan against the selected
    /// model and freezes its instruction, reasoning, and continuation decisions
    /// for the actor lifetime. When omitted, a construction-bound plan is
    /// compiled from the builder's instructions, request policy, and tools so
    /// every agent still carries exact plan identity for recovery.
    #[must_use]
    pub fn harness_plan(mut self, plan: ResolvedHarnessPlan) -> Self {
        self.harness_plan = Some(plan);
        self
    }

    /// Replaces time and identity sources for deterministic tests or embedding.
    #[must_use]
    pub fn identity_sources(mut self, clock: Arc<dyn Clock>, ids: Arc<dyn IdGenerator>) -> Self {
        self.clock = clock;
        self.ids = ids;
        self
    }

    /// Replaces the independent event identity source for deterministic tests.
    ///
    /// Event identities deliberately do not consume the canonical entry/turn/
    /// attempt source, so enabling observation cannot perturb model semantics.
    #[must_use]
    pub fn event_identity_source(mut self, ids: Arc<dyn IdGenerator>) -> Self {
        self.event_ids = ids;
        self
    }

    /// Replaces the immutable provider-neutral tool policy snapshot.
    #[must_use]
    pub fn tool_policy(mut self, policy: Arc<dyn ToolPolicy>) -> Self {
        self.policy = policy;
        self
    }

    /// Returns the exact immutable tool-policy revision selected for construction.
    #[must_use]
    pub fn tool_policy_revision(&self) -> ToolPolicyRevision {
        self.policy.revision()
    }

    /// Replaces the optional content-free telemetry emitter.
    ///
    /// Telemetry is disabled unless an emitter is supplied explicitly. Sink
    /// pressure, closure, and panics never affect actor execution.
    #[must_use]
    pub fn telemetry(mut self, telemetry: Arc<TelemetryEmitter>) -> Self {
        self.telemetry = telemetry;
        self
    }

    /// Starts the private actor and returns its cloneable command capability.
    ///
    /// # Errors
    ///
    /// Returns a typed error for invalid limits, exhausted identity sources, or
    /// construction outside a Tokio runtime. Mutating tools require a
    /// required-durability session store.
    pub fn build(self) -> Result<Agent, AgentError> {
        if self
            .tools
            .definitions()
            .iter()
            .any(|definition| definition.effect() != ToolEffect::ReadOnly)
        {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "mutating tools require a required-durability session store",
            ));
        }
        Ok(PreparedAgent::new(self)?.spawn())
    }

    /// Starts the private actor on one exact validated durable branch.
    ///
    /// This is the low-level handoff used by the high-level session workflow;
    /// callers cannot supply history, heads, or recovery state independently.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the builder's model, plan, tools, or prefix
    /// disagree with the validated durable checkpoint.
    pub(crate) async fn build_durable(
        self,
        bootstrap: DurableRuntimeBootstrap,
    ) -> Result<Agent, AgentError> {
        let store = Arc::clone(bootstrap.branch.store());
        let lease = bootstrap.branch.lease().clone();
        let cleanup_timeout = bootstrap.branch.store_timeouts().cleanup;
        match PreparedAgent::new_durable(self, bootstrap) {
            Ok(mut prepared) => match prepared.publish_recovered_turn().await {
                Ok(()) => Ok(prepared.spawn()),
                Err(error) => {
                    let lease = prepared
                        .authority
                        .durable_facts()
                        .map_or(lease, |branch| branch.lease().clone());
                    store_call(StorePhase::Release, cleanup_timeout, store.release(lease))
                        .await
                        .map_err(StoreCallError::into_agent)?;
                    Err(error)
                }
            },
            Err(error) => {
                store_call(StorePhase::Release, cleanup_timeout, store.release(lease))
                    .await
                    .map_err(StoreCallError::into_agent)?;
                Err(error)
            }
        }
    }
}

impl PreparedAgent {
    pub(super) fn new(builder: AgentBuilder) -> Result<Self, AgentError> {
        Self::new_bound(builder, None)
    }

    pub(super) fn new_durable(
        builder: AgentBuilder,
        bootstrap: DurableRuntimeBootstrap,
    ) -> Result<Self, AgentError> {
        Self::new_bound(builder, Some(bootstrap))
    }

    pub(super) fn new_bound(
        mut builder: AgentBuilder,
        bootstrap: Option<DurableRuntimeBootstrap>,
    ) -> Result<Self, AgentError> {
        let config = builder.config.validate()?;
        // This build closes such a turn itself, so it leaves no continuation
        // frame to restore.
        let local_terminal_recovery = bootstrap
            .as_ref()
            .is_some_and(|bootstrap| reopen_closes_working_turn(&bootstrap.checkpoint));
        let restored_frame = bootstrap
            .as_ref()
            .filter(|bootstrap| {
                !local_terminal_recovery
                    && bootstrap.checkpoint.kind() == crate::RuntimeCheckpointKind::Working
            })
            .map(|bootstrap| decode_restored_continuation(&bootstrap.checkpoint))
            .transpose()?;
        let durable_config = DurableAgentConfig::from_config(config)?;
        if restored_frame
            .as_ref()
            .is_some_and(|frame| frame.config() != durable_config)
        {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "builder runtime limits disagree with the durable continuation",
            ));
        }
        // A reopen that derives a different output bound than the session was
        // created under changes what the model may produce. The session log and
        // the operator both hear about it; it is never applied silently.
        let output_bound_rederived = bootstrap
            .as_ref()
            .and_then(|bootstrap| bootstrap.checkpoint.manifest().output_bound_rederived());
        let persisted_presentation =
            adopt_persisted_tool_semantics(&mut builder, bootstrap.as_ref())?;
        let model_error_target = validate_construction_inputs(&builder, bootstrap.as_ref())?;

        let ConstructionIdentities {
            agent: agent_id,
            session: session_id,
            branch: branch_id,
            run: run_id,
            close_event: close_event_id,
        } = construction_identities(&builder, bootstrap.as_ref(), restored_frame.as_ref())?;
        let (environment, job_sessions) = attach_job_environment(
            &builder,
            bootstrap.as_ref(),
            agent_id,
            session_id,
            branch_id,
            run_id,
        )?;
        let event_limits = subscription_limits(config)?;
        let telemetry_context = TelemetryContext::new(agent_id, session_id, branch_id, run_id);
        let tools = construction_tool_runtime(&mut builder, environment);
        let (harness_plan, request_policy, instructions, tool_exposure_map) =
            resolve_construction_harness_plan(&builder, &tools)?;
        let tool_presentation =
            construction_tool_presentation(&builder, tool_exposure_map.clone())?;
        if persisted_presentation.as_ref().is_some_and(|persisted| {
            std::mem::discriminant(persisted) != std::mem::discriminant(&tool_presentation)
                || persisted.digest() != tool_presentation.digest()
                || persisted.registry_digest() != tool_presentation.registry_digest()
        }) {
            return Err(AgentError::new(
                AgentErrorKind::IncompatibleSession,
                "builder presentation disagrees with durable tool semantics",
            ));
        }
        let stable_request_prefix = stable_prefix_from_effective_configuration(
            &builder.provider_configuration,
            &request_policy,
            instructions,
            &tool_exposure_map,
            &harness_plan,
        )?;
        let stable_request_prefix = tool_presentation.request_prefix(stable_request_prefix)?;
        let authority = if let Some(bootstrap) = bootstrap {
            reopened_checkpoint_authority(
                &builder,
                bootstrap,
                &stable_request_prefix,
                &harness_plan,
                &tools,
                &tool_presentation,
                restored_frame.is_some() || local_terminal_recovery,
            )?
        } else {
            fresh_checkpoint_authority(
                &builder,
                (session_id, branch_id, run_id),
                &stable_request_prefix,
                &harness_plan,
                &tools,
                &tool_presentation,
            )?
        };
        let (initial_command_ledger, next_command_sequence, next_operation_sequence) =
            rehydrate_runtime_indexes(
                &authority.checkpoint(),
                &BTreeMap::new(),
                authority.durable_facts().is_some(),
            )?;
        let interpreter_observation = reopened_interpreter_observation(&builder, &authority)?;
        let authoritative_conversation = authority.conversation();
        let events = EventHub::new(
            agent_id,
            session_id,
            branch_id,
            run_id,
            Arc::clone(&builder.clock),
            Arc::clone(&builder.event_ids),
            close_event_id,
            &authoritative_conversation,
            EventHubLimits::new(
                config.event_capacity,
                config.event_byte_capacity,
                config.max_event_subscribers,
            ),
        )
        .map_err(event_error)?;

        let restored = restored_frame
            .map(|frame| restore_startup(&builder, frame, run_id, &tools, &tool_presentation))
            .transpose()?;
        Ok(Self {
            job_sessions,
            job_host: builder.job_host,
            model: builder.model,
            provider_configuration: builder.provider_configuration,
            model_error_target,
            tools,
            tool_presentation,
            config,
            stable_request_prefix,
            harness_plan,
            clock: builder.clock,
            ids: builder.ids,
            agent_id,
            session_id,
            branch_id,
            run_id,
            authority,
            initial_command_ledger,
            next_command_sequence,
            next_operation_sequence,
            events,
            event_limits,
            output_bound_rederived,
            // Set by `publish_recovered_turn`, which runs after construction.
            abandoned_turn: None,
            telemetry: builder.telemetry,
            telemetry_context,
            restored,
            local_composition: builder.local_composition,
            interpreter_observation,
            post_compaction_note: builder.post_compaction_note,
            request_slots: builder.request_slots,
            model_rebind: builder
                .model_reresolver
                .map(|(reresolver, admitted_until)| {
                    super::reresolution::ModelRebind::new(
                        reresolver,
                        admitted_until,
                        &builder.initial_model_authority,
                    )
                }),
        })
    }

    /// Closes the exact durable root before admission, then rehydrates indexes.
    pub(super) async fn publish_recovered_turn(&mut self) -> Result<(), AgentError> {
        let telemetry = DurabilityCommitTelemetry {
            emitter: Arc::clone(&self.telemetry),
            clock: Arc::clone(&self.clock),
            context: self.telemetry_context,
        };
        let workflow = WorkflowClosureRuntime {
            model: Arc::clone(&self.model),
            configuration: &self.provider_configuration,
            clock: self.clock.as_ref(),
            authority_timeout: self.config.model_operation_timeout,
            post_compaction_note: self.post_compaction_note.as_ref(),
            ids: self.ids.as_ref(),
        };
        let recovery = Box::pin(
            self.authority
                .publish_root_turn_closure(&telemetry, workflow),
        )
        .await?;
        if let Some(recovery) = recovery {
            self.abandoned_turn = recovery.abandoned_turn;
        }
        let checkpoint = self.authority.checkpoint();
        let durable = self.authority.durable_facts();
        let feed_sequences = if let Some(branch) = durable.as_ref() {
            Box::pin(
                crate::runtime::continuation::command_feed::command_feed_sequences(
                    &checkpoint,
                    branch,
                ),
            )
            .await?
        } else {
            BTreeMap::new()
        };
        let (command_ledger, next_command_sequence, next_operation_sequence) =
            rehydrate_runtime_indexes(&checkpoint, &feed_sequences, durable.is_some())?;
        self.initial_command_ledger = command_ledger;
        self.next_command_sequence = next_command_sequence;
        self.next_operation_sequence = next_operation_sequence;
        Ok(())
    }
}

/// Adopts the persisted harness plan, job environment and tool presentation of
/// a reopened session, then checks any local composition against the tool
/// semantics it will run with.
fn adopt_persisted_tool_semantics(
    builder: &mut AgentBuilder,
    bootstrap: Option<&DurableRuntimeBootstrap>,
) -> Result<Option<ToolPresentation>, AgentError> {
    let persisted_presentation = if let Some(bootstrap) = bootstrap {
        let persisted_plan = bootstrap
            .checkpoint
            .manifest()
            .rehydrate_harness_plan()
            .map_err(runtime_manifest_error)?;
        if builder
            .harness_plan
            .as_ref()
            .is_some_and(|provided| provided != &persisted_plan)
        {
            return Err(AgentError::new(
                AgentErrorKind::IncompatibleSession,
                "builder harness plan disagrees with durable semantics",
            ));
        }
        builder.harness_plan = Some(persisted_plan);
        builder.job_config = stored_job_config(
            bootstrap
                .checkpoint
                .manifest()
                .tools()
                .environment()
                .bytes(),
        )?;
        let (registry, presentation) =
            reconstruct_persisted_tools(bootstrap.checkpoint.manifest(), &builder.tool_bindings)?;
        validate_reopened_projection(builder.tool_projection.as_ref(), &presentation)?;
        builder.tools = registry;
        builder.tool_projection = match &presentation {
            ToolPresentation::V1(_) => None,
            ToolPresentation::V2(projection) => Some(projection.as_ref().clone()),
        };
        if let Some(local_composition) = builder.local_composition.as_mut() {
            let target = builder
                .tool_projection
                .as_ref()
                .ok_or_else(|| {
                    AgentError::new(
                        AgentErrorKind::InvalidInput,
                        "durable local composition reconstruction has no V2 outer projection",
                    )
                })?
                .target()
                .clone();
            let reconstructed = local_composition
                .integration
                .rebuild_descriptor(builder.job_config, builder.tools.clone(), target)
                .map_err(|error| AgentError::new(AgentErrorKind::InvalidInput, error))?;
            if reconstructed.identity != local_composition.descriptor.identity {
                return Err(AgentError::new(
                    AgentErrorKind::InvalidInput,
                    "durable local composition descriptor disagrees with reconstructed tool semantics",
                ));
            }
            local_composition.descriptor = reconstructed;
        }
        Some(presentation)
    } else {
        None
    };
    if persisted_presentation.is_none()
        && let Some(local_composition) = builder.local_composition.as_mut()
    {
        let target = local_composition
            .descriptor
            .outer_projection
            .target()
            .clone();
        let reconstructed = local_composition
            .integration
            .rebuild_descriptor(builder.job_config, builder.tools.clone(), target)
            .map_err(|error| AgentError::new(AgentErrorKind::InvalidInput, error))?;
        if reconstructed.identity != local_composition.descriptor.identity {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "fresh local composition descriptor disagrees with reconstructed tool semantics",
            ));
        }
        local_composition.descriptor = reconstructed;
    }
    Ok(persisted_presentation)
}

/// Validates the builder against the effective provider configuration and
/// the construction environment; returns the public error attribution target.
fn validate_construction_inputs(
    builder: &AgentBuilder,
    bootstrap: Option<&DurableRuntimeBootstrap>,
) -> Result<ModelErrorTarget, AgentError> {
    let runtime_durability = bootstrap.map_or(RuntimeDurability::Disabled, |bootstrap| {
        bootstrap.checkpoint.manifest().durability()
    });
    validate_configuration_runtime_authority(
        &builder.provider_configuration,
        builder.provider_configuration.effective().descriptor(),
        &builder.initial_model_authority,
        builder.clock.now(),
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "initial model authority disagrees with the effective provider configuration",
        )
    })?;
    let behavior = builder.provider_configuration.effective().behavior();
    if !matches!(
        (behavior.local_durability, runtime_durability),
        (
            LocalDurabilityDisposition::Ephemeral,
            RuntimeDurability::Disabled
        ) | (
            LocalDurabilityDisposition::BestEffort,
            RuntimeDurability::BestEffort
        ) | (
            LocalDurabilityDisposition::DurableRequired,
            RuntimeDurability::Required
        )
    ) {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "configured provider tools or durability disagree with agent construction",
        ));
    }
    let model_error_target =
        ModelErrorTarget::from_descriptor(builder.provider_configuration.effective().descriptor())
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::InvalidInput,
                    "resolved model identities are unsafe for public error attribution",
                )
            })?;
    if builder.instructions.len() > 256 {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "agent instruction prefix exceeds the canonical block limit",
        ));
    }
    tokio::runtime::Handle::try_current().map_err(|_| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "agent construction requires an active Tokio runtime",
        )
    })?;
    Ok(model_error_target)
}

struct ConstructionIdentities {
    agent: AgentId,
    session: SessionId,
    branch: BranchId,
    run: RunId,
    close_event: EventId,
}

/// Mints the agent identity and adopts or mints the session, branch and run.
fn construction_identities(
    builder: &AgentBuilder,
    bootstrap: Option<&DurableRuntimeBootstrap>,
    restored_frame: Option<&RestoredContinuationFrame>,
) -> Result<ConstructionIdentities, AgentError> {
    let agent_id = AgentId::generate(builder.clock.as_ref(), builder.ids.as_ref())
        .map_err(|error| AgentError::from_identity(&error))?;
    let (session_id, branch_id, run_id) = if let Some(bootstrap) = bootstrap {
        let session = bootstrap.branch.head().session();
        (
            session.session_id(),
            session.branch_id(),
            restored_frame.map_or(bootstrap.branch.lease().owner(), |frame| {
                frame.provider_request().metadata().run_id()
            }),
        )
    } else {
        (
            SessionId::generate(builder.clock.as_ref(), builder.ids.as_ref())
                .map_err(|error| AgentError::from_identity(&error))?,
            BranchId::generate(builder.clock.as_ref(), builder.ids.as_ref())
                .map_err(|error| AgentError::from_identity(&error))?,
            RunId::generate(builder.clock.as_ref(), builder.ids.as_ref())
                .map_err(|error| AgentError::from_identity(&error))?,
        )
    };
    let close_event_id = EventId::generate(builder.clock.as_ref(), builder.event_ids.as_ref())
        .map_err(|_| event_error(ViewError::Identity))?;
    Ok(ConstructionIdentities {
        agent: agent_id,
        session: session_id,
        branch: branch_id,
        run: run_id,
        close_event: close_event_id,
    })
}

/// Builds the run environment and joins it to the job host, restoring the
/// persisted job counter and terminal job rows of a reopened session.
fn attach_job_environment(
    builder: &AgentBuilder,
    bootstrap: Option<&DurableRuntimeBootstrap>,
    agent_id: AgentId,
    session_id: SessionId,
    branch_id: BranchId,
    run_id: RunId,
) -> Result<(ExecutionEnvironment, plakous_tools::JoinedProcessSessions), AgentError> {
    let environment = run_environment(
        &builder.workspace,
        run_id,
        builder.host_process_options.clone(),
        builder.job_config,
    )?;
    let fresh_sessions = environment
        .joined_process_sessions()
        .map_err(|error| AgentError::from_tool(&plakous_tools::backend_runtime_error(error)))?;
    plakous_tools::internal::seed_job_counter(&fresh_sessions, builder.job_counter_seed);
    let job_sessions = if let Some(host) = &builder.job_host {
        host.attach(
            plakous_tools::JobOwner {
                session_id,
                branch_id,
                agent_id,
            },
            fresh_sessions,
        )
        .map_err(|error| AgentError::from_tool(&plakous_tools::backend_runtime_error(error)))?
    } else {
        fresh_sessions
    };
    plakous_tools::internal::set_job_owner(&job_sessions, agent_id.to_string());
    if let Some(bootstrap) = bootstrap {
        plakous_tools::internal::seed_job_counter(
            &job_sessions,
            bootstrap.checkpoint.manifest().jobs().next_id(),
        );
        let rows = bootstrap.checkpoint.manifest().jobs().rows();
        let unmatched = job_sessions
            .jobs()
            .iter()
            .filter(|view| {
                view.status != plakous_tools::JobStatus::Running
                    && !rows.iter().any(|row| {
                        row.number == view.number
                            && row.session_id.as_bytes() == view.session_id
                            && row.owner_generation.get() == view.owner_generation
                    })
            })
            .map(|view| view.number)
            .collect::<Vec<_>>();
        plakous_tools::internal::consume_jobs(&job_sessions, &unmatched);
        plakous_tools::internal::restore_job_terminals(
            &job_sessions,
            &bootstrap
                .checkpoint
                .manifest()
                .jobs()
                .rows()
                .iter()
                .map(crate::manifest::JobRow::view)
                .collect::<Vec<_>>(),
        );
    }
    environment.adopt_joined_process_sessions(&job_sessions);
    Ok((environment, job_sessions))
}

fn construction_tool_runtime(
    builder: &mut AgentBuilder,
    environment: ExecutionEnvironment,
) -> ToolRuntime {
    match builder.tool_scheduler.take() {
        Some((scheduler, config)) => ToolRuntime::with_scheduler(
            builder.tools.clone(),
            Arc::new(environment),
            Arc::clone(&builder.policy),
            scheduler,
            config,
        ),
        None => ToolRuntime::new(
            builder.tools.clone(),
            Arc::new(environment),
            Arc::clone(&builder.policy),
        ),
    }
}
