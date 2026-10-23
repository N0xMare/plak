//! Prepare exact runtime configuration and owned resources before actor publication.

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

pub(super) fn tool_registry_descriptor(
    component: &ReconstructiveComponent,
) -> Result<FrozenToolRegistryDescriptor, AgentError> {
    match component.codec_revision() {
        TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION => {
            FrozenToolRegistryDescriptor::decode_v1(component.bytes())
        }
        TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION => {
            FrozenToolRegistryDescriptor::decode_v2(component.bytes())
        }
        _ => {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "rebind source tool-registry codec is unsupported",
            ));
        }
    }
    .map_err(tool_descriptor_error)
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

pub(super) fn validate_local_composition_binding_slot(
    already_bound: bool,
) -> Result<(), AgentError> {
    if already_bound {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "an agent may bind only one local composition profile",
        ));
    }
    Ok(())
}

pub(super) fn local_composition_binding_identity(
    attachment: &crate::local_composition::LocalCompositionAttachmentIdentity,
) -> Result<Digest, AgentError> {
    let profile = attachment.profile.identity_bytes();
    let artifact = attachment.artifact.identity_bytes();
    let engine_contract = attachment.engine_contract.identity_bytes();
    let evidence = attachment
        .evidence_receipt
        .map(|value| value.identity_bytes());
    let mut parts = vec![
        b"plakous.local-composition.attachment.v1".as_slice(),
        profile.as_slice(),
        artifact.as_slice(),
        engine_contract.as_slice(),
    ];
    if let Some(value) = evidence.as_ref() {
        parts.push(value.as_slice());
    }
    Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &parts).map_err(|_| {
        AgentError::new(
            AgentErrorKind::Identity,
            "local composition binding identity could not be validated",
        )
    })
}

pub(super) fn validate_local_composition_attachment_identity(
    attachment: &crate::local_composition::LocalCompositionAttachmentIdentity,
) -> Result<(), AgentError> {
    if attachment.binding != local_composition_binding_identity(attachment)? {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "local composition binding identity is invalid",
        ));
    }
    Ok(())
}

impl ToolPresentation {
    /// The canonical name a call's exposed name resolves to (spec 01 §8.2).
    ///
    /// An exposure whose operations share one canonical name resolves without
    /// the call's arguments. One that lowers to an operation by its arguments
    /// resolves through `prepare`, exactly as admission will, and to its first
    /// operation when the call does not prepare. A name nothing exposes
    /// resolves to itself.
    pub(super) fn canonical_name(
        &self,
        call: &ToolCallBlock,
        prepare: impl FnOnce(&Self) -> Result<PreparedToolInvocation, ToolRuntimeError>,
    ) -> ToolName {
        let exposed = call.exposed_name();
        let resolved = match self {
            Self::V1(exposure) => exposure
                .canonical_identity(exposed)
                .map(|identity| identity.name().clone()),
            Self::V2(projection) => projection.by_exposed(exposed).and_then(|entry| {
                let mut names = entry
                    .canonical_operations()
                    .values()
                    .map(plakous_tools::RegisteredToolIdentity::name);
                let first = names.next()?.clone();
                if names.all(|name| *name == first) {
                    return Some(first);
                }
                Some(prepare(self).map_or(first, |prepared| {
                    prepared.invocation.identity().name().clone()
                }))
            }),
        };
        resolved.unwrap_or_else(|| exposed.clone())
    }

    /// Whether `name` presents a composition `wait` marker, judged by the
    /// canonical identity it resolves to, never by its spelling.
    pub(super) fn is_cell_wait_marker(&self, name: &ToolName) -> bool {
        match self {
            Self::V1(exposure) => exposure
                .canonical_identity(name)
                .is_some_and(plakous_tools::is_cell_wait_marker),
            Self::V2(projection) => projection.by_exposed(name).is_some_and(|entry| {
                entry
                    .canonical_operations()
                    .values()
                    .all(plakous_tools::is_cell_wait_marker)
            }),
        }
    }

    pub(super) fn registry_arguments<'a>(
        &self,
        call: &'a ToolCallBlock,
    ) -> Result<&'a str, ToolRuntimeError> {
        let custom = match self {
            Self::V1(exposure) => exposure.definitions().iter().any(|definition| {
                definition.name() == call.exposed_name()
                    && matches!(definition.input(), ToolInputContract::Utf8(_))
            }),
            Self::V2(projection) => {
                projection
                    .by_exposed(call.exposed_name())
                    .is_some_and(|entry| {
                        matches!(
                            entry.presentation(),
                            ProjectedToolPresentationV2::CustomUtf8 { .. }
                        )
                    })
            }
        };
        if !custom {
            return Ok(call.raw_arguments());
        }
        if let Some(serde_json::Value::String(text)) = call.arguments() {
            Ok(text)
        } else {
            Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "custom tool input is not a canonical JSON string",
            ))
        }
    }

    pub(super) fn prepare(
        &self,
        request: ToolInvocationRequest,
        environment: &ExecutionEnvironment,
    ) -> Result<PreparedToolInvocation, ToolRuntimeError> {
        match self {
            Self::V1(exposure) => {
                exposure
                    .prepare(request)
                    .map(|invocation| PreparedToolInvocation {
                        invocation,
                        projection_receipt: None,
                    })
            }
            Self::V2(projection) => {
                projection
                    .prepare_in_environment(request, environment)
                    .map(|prepared| {
                        let (invocation, receipt) = prepared.into_parts();
                        PreparedToolInvocation {
                            invocation,
                            projection_receipt: receipt,
                        }
                    })
            }
        }
    }

    pub(super) fn raise(
        &self,
        receipt: Option<&ToolProjectionReceiptV2>,
        outcome: ToolOutcome,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        match (self, receipt) {
            (Self::V2(projection), Some(receipt)) => projection.raise(receipt, outcome),
            (Self::V1(_) | Self::V2(_), None) => Ok(outcome),
            _ => Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "tool projection receipt disagrees with the runtime presentation mode",
            )),
        }
    }

    pub(super) fn request_prefix(
        &self,
        prefix: StableRequestPrefix,
    ) -> Result<StableRequestPrefix, AgentError> {
        match self {
            Self::V1(_) => Ok(prefix),
            Self::V2(projection) => projection
                .compile_request_view_v2()
                .and_then(|view| {
                    prefix.with_tool_projection(view).map_err(|error| {
                        ToolRuntimeError::new(
                            ToolRuntimeErrorKind::Registration,
                            format!(
                                "stable request prefix rejected the exact V2 projection ({error})"
                            ),
                        )
                    })
                })
                .map_err(|error| AgentError::from_tool(&error)),
        }
    }

    pub(super) fn registry_digest(&self) -> Digest {
        match self {
            Self::V1(exposure) => exposure.registry_digest(),
            Self::V2(projection) => projection.registry_digest(),
        }
    }

    pub(super) fn digest(&self) -> Digest {
        match self {
            Self::V1(exposure) => exposure.digest(),
            Self::V2(projection) => projection.digest(),
        }
    }
}

pub(super) fn validate_reopened_projection(
    provided: Option<&ToolProjectionMap>,
    presentation: &ToolPresentation,
) -> Result<(), AgentError> {
    let message = match (provided, presentation) {
        (Some(provided), ToolPresentation::V2(persisted))
            if provided.digest() == persisted.digest()
                && provided.registry_digest() == persisted.registry_digest() =>
        {
            return Ok(());
        }
        (None, _) => return Ok(()),
        (Some(provided), ToolPresentation::V2(persisted))
            if provided.target() != persisted.target() =>
        {
            "builder projection target disagrees with durable provider configuration"
        }
        (Some(_), ToolPresentation::V2(_)) => {
            "builder projection registry or exposure disagrees with durable tool semantics"
        }
        _ => "builder projection mode disagrees with durable tool semantics",
    };
    Err(AgentError::new(AgentErrorKind::InvalidInput, message))
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

/// Admits a reopened checkpoint as the authority after checking that this
/// build reproduces its binding, request prefix and harness plan.
fn reopened_checkpoint_authority(
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
fn fresh_checkpoint_authority(
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
        builder.job_counter_seed,
    )?;
    let bundle =
        RuntimeCheckpointCodec::bundle(&checkpoint, None).map_err(checkpoint_bundle_error)?;
    Ok(PreparedCheckpoint::new(Arc::new(checkpoint), bundle, None))
}

/// A persistent interpreter restarts on every reopen: the next generation is
/// recorded as reset by the process reopen.
fn reopened_interpreter_observation(
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
fn restore_startup(
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

pub(super) fn resolve_construction_harness_plan(
    builder: &AgentBuilder,
    tools: &ToolRuntime,
) -> Result<
    (
        ResolvedHarnessPlan,
        AgentRequestPolicy,
        Vec<InstructionBlock>,
        ToolExposureMap,
    ),
    AgentError,
> {
    let registry_digest = tools.registry_identity_digest().map_err(|error| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            format!("frozen tool registry could not be digested: {error}"),
        )
    })?;
    let canonical_tool_names = tools
        .definitions()
        .iter()
        .map(|definition| definition.name().as_str().to_owned())
        .collect::<Vec<_>>();
    // Plan compilation must use run-stable workspace facts, never the
    // run-scoped execution-environment allocation identity. Recovery reopens
    // mint a new RunId and would otherwise invalidate plan digests.
    let environment_digest = stable_workspace_environment_digest(builder.workspace.as_ref())?;

    let plan = if let Some(plan) = builder.harness_plan.as_ref() {
        let model_facts = compiler_model_facts_from_model(
            builder.model.as_ref(),
            &builder.provider_configuration,
        )
        .map_err(|error| profile_construction_error(&error))?;
        plan.ensure_model_binding(&model_facts)
            .map_err(|error| profile_construction_error(&error))?;
        plan.clone()
    } else {
        compile_construction_bound_plan(
            builder.model.as_ref(),
            &builder.provider_configuration,
            registry_digest,
            canonical_tool_names,
            environment_digest,
            &builder.instructions,
            builder.request_policy.reasoning(),
            true,
        )
        .map_err(|error| profile_construction_error(&error))?
    };
    if plan.tool_registry_digest() != registry_digest
        || plan.execution_environment_digest() != environment_digest
    {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "resolved harness plan is bound to different tool or execution-environment facts",
        ));
    }
    let derived_exposure = derive_plan_tool_exposure(&builder.tools, &plan)?;
    let tool_exposure_map = derived_exposure;

    // Explicit plans own model-facing instruction/reasoning decisions.
    // Construction-bound plans record those decisions for identity only and
    // must not rewrite the caller's exact request policy or instruction prefix.
    if builder.harness_plan.is_some() {
        let request_policy = builder
            .request_policy
            .clone()
            .with_reasoning(plan.reasoning_policy());
        let instructions = plan
            .instruction_blocks()
            .map_err(|error| profile_construction_error(&error))?;
        Ok((plan, request_policy, instructions, tool_exposure_map))
    } else {
        Ok((
            plan,
            builder.request_policy.clone(),
            builder.instructions.clone(),
            tool_exposure_map,
        ))
    }
}

fn effective_request_policy(
    configuration: &CompiledProviderConfiguration,
) -> Result<AgentRequestPolicy, AgentError> {
    let behavior = configuration.effective().behavior();
    let reasoning = match behavior.reasoning_effort {
        ReasoningEffortDisposition::Disabled => ReasoningPolicy::Disabled,
        ReasoningEffortDisposition::Enabled(effort) => ReasoningPolicy::Enabled {
            effort: Some(effort),
            requirement: CapabilityRequirement::Required,
        },
    };
    let max_output_tokens = behavior.output_bound.bound();
    let limits = GenerationLimits::new(max_output_tokens, Vec::new()).map_err(|_| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "effective output bound could not be represented in the model request",
        )
    })?;
    Ok(AgentRequestPolicy::new(limits, reasoning)
        .with_context_admission_requirement(behavior.context_evidence_requirement))
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

pub(super) fn stable_prefix_from_effective_configuration(
    configuration: &CompiledProviderConfiguration,
    request_policy: &AgentRequestPolicy,
    instructions: Vec<InstructionBlock>,
    exposure: &ToolExposureMap,
    harness_plan: &ResolvedHarnessPlan,
) -> Result<StableRequestPrefix, AgentError> {
    let behavior = configuration.effective().behavior();
    let expected_policy = effective_request_policy(configuration)?;
    let configured_tools = behavior.tools.clone();
    let expected_cache = if harness_plan.cache().prefer_stable_prefix() {
        CachePolicy::Enabled {
            requirement: CapabilityRequirement::Required,
        }
    } else {
        CachePolicy::Disabled
    };
    if request_policy != &expected_policy {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "effective provider behavior disagrees with the agent request policy",
        ));
    }
    if (!exposure.definitions().is_empty()
        && !configured_tools
            .iter()
            .map(plakous_model::ConfiguredTool::definition)
            .eq(exposure.definitions()))
        || behavior
            .tools
            .iter()
            .any(|tool| tool.execution() != ToolExecutionDisposition::Local)
    {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "effective provider tool behavior disagrees with the agent tool exposure",
        ));
    }
    if behavior.cache != expected_cache {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "effective provider cache behavior disagrees with the harness plan",
        ));
    }
    StableRequestPrefix::new(
        configuration.selected_identity(),
        configuration.effective_identity(),
        behavior.processing_tier.clone(),
        instructions,
        configured_tools,
        behavior.hosted_tools.clone(),
        behavior.output.clone(),
        expected_policy.reasoning(),
        behavior.tool_choice.clone(),
        behavior.cache,
        expected_policy.generation_limits().clone(),
        NativeOptions::default(),
        Some(*harness_plan.plan_digest()),
    )
    .and_then(|prefix| {
        prefix.with_context_admission_requirement(expected_policy.context_admission_requirement())
    })
    .map_err(|error| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            format!("stable model-request prefix is invalid: {error}"),
        )
    })
}

pub(super) fn construction_tool_presentation(
    builder: &AgentBuilder,
    exposure: ToolExposureMap,
) -> Result<ToolPresentation, AgentError> {
    match builder.tool_projection.as_ref() {
        Some(projection) => {
            let admission = if let Some(local_composition) = builder.local_composition.as_ref() {
                local_composition.integration.validate_configuration(
                    builder.job_config,
                    &local_composition.descriptor,
                    projection,
                    builder.tools.clone(),
                    &builder.provider_configuration,
                )
            } else if projection.target().integration_class()
                == plakous_model::IntegrationClass::Custom
            {
                validate_custom_process_presentation_v2(
                    projection,
                    &builder.tools,
                    &builder.provider_configuration,
                )
                .map_err(|error| error.to_string())
            } else {
                builder
                    .projection_admission
                    .as_ref()
                    .ok_or_else(|| {
                        "a standard-integration V2 projection needs a candidate admission from \
                         the composition root"
                            .to_owned()
                    })
                    .and_then(|admission| {
                        admission
                            .admit(
                                builder.job_config,
                                projection,
                                &builder.tools,
                                &builder.provider_configuration,
                            )
                            .map_err(|error| error.to_string())
                    })
            };
            admission.map_err(|error| {
                AgentError::new(
                    AgentErrorKind::InvalidInput,
                    format!(
                        "V2 projection is not qualified for the exact provider configuration: {error}"
                    ),
                )
            })?;
            Ok(ToolPresentation::V2(Arc::new(projection.clone())))
        }
        None => Ok(ToolPresentation::V1(exposure)),
    }
}

fn derive_plan_tool_exposure(
    registry: &FrozenToolRegistry,
    plan: &ResolvedHarnessPlan,
) -> Result<ToolExposureMap, AgentError> {
    let planned = plan.tools().exposed_names();
    if !planned.is_empty() && planned.len() != registry.definitions().len() {
        return Err(profile_construction_error(
            &ProfileError::PlanToolExposureMismatch,
        ));
    }
    if planned.is_empty() {
        return ToolExposureMap::new(registry.clone(), []).map_err(|error| {
            AgentError::new(
                AgentErrorKind::InvalidInput,
                format!("resolved tool exposure map is invalid: {error}"),
            )
        });
    }
    let exposures = registry
        .definitions()
        .iter()
        .map(|definition| {
            let identity = registry
                .identity(definition.name())
                .cloned()
                .ok_or_else(|| {
                    AgentError::new(
                        AgentErrorKind::InvalidInput,
                        "resolved plan references a tool without executable identity",
                    )
                })?;
            let override_entry = plan
                .tools()
                .overrides()
                .iter()
                .rev()
                .find(|entry| entry.canonical_name() == definition.name().as_str());
            let exposed_name = override_entry
                .map_or_else(|| definition.name().as_str(), |entry| entry.exposed_name());
            ToolExposure::new(identity, exposed_name.to_owned())
                .and_then(|exposure| {
                    exposure.with_description_override(
                        override_entry
                            .and_then(|entry| entry.description())
                            .map(str::to_owned),
                    )
                })
                .map_err(|error| {
                    AgentError::new(
                        AgentErrorKind::InvalidInput,
                        format!("resolved tool exposure is invalid: {error}"),
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut derived_names = exposures
        .iter()
        .map(|exposure| exposure.exposed_name().as_str())
        .collect::<Vec<_>>();
    let mut planned_names = planned.iter().map(String::as_str).collect::<Vec<_>>();
    derived_names.sort_unstable();
    planned_names.sort_unstable();
    if derived_names != planned_names {
        return Err(profile_construction_error(
            &ProfileError::PlanToolExposureMismatch,
        ));
    }
    ToolExposureMap::new(registry.clone(), exposures).map_err(|error| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            format!("resolved tool exposure map is invalid: {error}"),
        )
    })
}

fn stable_workspace_environment_digest(workspace: &WorkspacePolicy) -> Result<Digest, AgentError> {
    let bytes = stable_workspace_environment_bytes(workspace)?;
    Digest::sha256(
        DigestDomain::ExecutionEnvironment,
        1,
        &[b"plakous-agent/workspace-facts-v1", &bytes],
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "workspace environment facts could not be digested",
        )
    })
}

/// Compiles an explicit profile from the exact tool registry and workspace
/// policy that an [`AgentBuilder`] will consume.
///
/// This is the safe consumer seam for explicit product profiles: callers do
/// not reconstruct private tool or execution-environment identity encodings.
/// The resulting plan can be supplied directly to [`AgentBuilder::harness_plan`].
///
/// # Errors
///
/// Returns a typed construction error when the registry or workspace cannot
/// be identified, or when the profile does not compile for the selected model.
pub fn compile_explicit_runtime_profile(
    spec: &HarnessProfileSpec,
    model: &dyn LanguageModel,
    provider_configuration: &CompiledProviderConfiguration,
    tools: &FrozenToolRegistry,
    workspace: &WorkspacePolicy,
    context_sources: Vec<ResolvedContextSourceFact>,
) -> Result<ResolvedHarnessPlan, AgentError> {
    let registry_digest = tools.registry_identity_digest().map_err(|error| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            format!("frozen tool registry could not be digested: {error}"),
        )
    })?;
    let canonical_tool_names = tools
        .definitions()
        .iter()
        .map(|definition| definition.name().as_str().to_owned())
        .collect();
    let environment_digest = stable_workspace_environment_digest(workspace)?;
    crate::compile_explicit_profile(
        spec,
        model,
        provider_configuration,
        registry_digest,
        canonical_tool_names,
        environment_digest,
        context_sources,
    )
    .map_err(|error| profile_construction_error(&error))
}

pub(super) fn stable_runtime_environment_component(
    workspace: &WorkspacePolicy,
    local_composition: Option<&LocalCompositionConstruction>,
    provider_configuration: &CompiledProviderConfiguration,
    job_config: plakous_tools::JobConfig,
) -> Result<ReconstructiveComponent, AgentError> {
    let mut bytes = stable_workspace_environment_bytes(workspace)?;
    let Some(local_composition) = local_composition else {
        append_job_config(&mut bytes, job_config);
        return runtime_component(RuntimeComponentKind::ExecutionEnvironment, bytes);
    };
    bytes.extend_from_slice(b"\0plakous-agent/local-composition-attachment-v2\0");
    for identity in [
        local_composition.attachment.profile,
        local_composition.attachment.artifact,
        local_composition.attachment.binding,
        local_composition.attachment.engine_contract,
        local_composition.descriptor.identity,
        local_composition.descriptor.outer_projection.digest(),
        local_composition.descriptor.nested_projection.digest(),
        provider_configuration.selected_identity().digest(),
        provider_configuration.effective_identity().digest(),
    ] {
        bytes.extend_from_slice(&identity.identity_bytes());
    }
    bytes.extend_from_slice(
        &local_composition
            .integration
            .max_yield_time_ms()
            .to_be_bytes(),
    );
    bytes.extend_from_slice(
        &u64::try_from(local_composition.integration.max_output_tokens())
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::InvalidInput,
                    "local composition output limit exceeds the runtime bound",
                )
            })?
            .to_be_bytes(),
    );
    let host = local_composition.integration.environment_identity();
    bytes.extend_from_slice(
        &u64::try_from(host.len())
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::InvalidInput,
                    "local composition host identity exceeds the runtime bound",
                )
            })?
            .to_be_bytes(),
    );
    bytes.extend_from_slice(&host);
    append_job_config(&mut bytes, job_config);
    runtime_component(RuntimeComponentKind::ExecutionEnvironment, bytes)
}

const JOB_CONFIG_ENVIRONMENT_TAG: &[u8] = b"\0plakous-agent/job-limits-v1\0";

pub(crate) fn stored_composition_artifact(
    mut bytes: &[u8],
) -> Result<Option<crate::facade_support::StoredCompositionArtifact>, AgentError> {
    fn take<'a>(bytes: &mut &'a [u8], len: usize) -> Option<&'a [u8]> {
        let value = bytes.get(..len)?;
        *bytes = &bytes[len..];
        Some(value)
    }
    fn framed<'a>(bytes: &mut &'a [u8]) -> Option<&'a [u8]> {
        let len = u64::from_be_bytes(take(bytes, 8)?.try_into().ok()?);
        take(bytes, usize::try_from(len).ok()?)
    }
    fn digest(bytes: &mut &[u8]) -> Option<Vec<u8>> {
        let original = *bytes;
        framed(bytes)?;
        framed(bytes)?;
        take(bytes, 36)?;
        Some(original[..original.len() - bytes.len()].to_vec())
    }
    let invalid = || {
        AgentError::new(
            AgentErrorKind::IncompatibleSession,
            "stored composition environment is invalid",
        )
    };
    let prefix = b"plakous-agent/workspace-environment-v1\0";
    if take(&mut bytes, prefix.len()) != Some(prefix.as_slice()) {
        return Err(invalid());
    }
    let os_end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(invalid)?;
    take(&mut bytes, os_end + 1).ok_or_else(invalid)?;
    let policy = b"host-path-policy-only\0";
    if take(&mut bytes, policy.len()) != Some(policy.as_slice()) {
        return Err(invalid());
    }
    framed(&mut bytes).ok_or_else(invalid)?;
    framed(&mut bytes).ok_or_else(invalid)?;
    if bytes.starts_with(JOB_CONFIG_ENVIRONMENT_TAG) {
        if bytes.len() != JOB_CONFIG_ENVIRONMENT_TAG.len() + 16 {
            return Err(invalid());
        }
        stored_job_config(bytes)?;
        return Ok(None);
    }
    let tag = b"\0plakous-agent/local-composition-attachment-v2\0";
    if take(&mut bytes, tag.len()) != Some(tag.as_slice()) {
        return Err(invalid());
    }
    let profile_identity = digest(&mut bytes).ok_or_else(invalid)?;
    let artifact_identity = digest(&mut bytes).ok_or_else(invalid)?;
    let binding_identity = digest(&mut bytes).ok_or_else(invalid)?;
    let engine_contract_identity = digest(&mut bytes).ok_or_else(invalid)?;
    for _ in 0..5 {
        digest(&mut bytes).ok_or_else(invalid)?;
    }
    take(&mut bytes, 16).ok_or_else(invalid)?; // presentation limits
    let host = framed(&mut bytes).ok_or_else(invalid)?.to_vec();
    if bytes.len() != JOB_CONFIG_ENVIRONMENT_TAG.len() + 16 {
        return Err(invalid());
    }
    stored_job_config(bytes)?;
    Ok(Some(crate::facade_support::StoredCompositionArtifact {
        profile_identity,
        artifact_identity,
        binding_identity,
        engine_contract_identity,
        host,
    }))
}

fn append_job_config(bytes: &mut Vec<u8>, config: plakous_tools::JobConfig) {
    bytes.extend_from_slice(JOB_CONFIG_ENVIRONMENT_TAG);
    bytes.extend_from_slice(&config.foreground_seconds().to_be_bytes());
    bytes.extend_from_slice(&config.max_runtime_hours().to_be_bytes());
}

pub(crate) fn stored_job_config(bytes: &[u8]) -> Result<plakous_tools::JobConfig, AgentError> {
    let invalid = || {
        AgentError::new(
            AgentErrorKind::IncompatibleSession,
            "stored job environment is invalid",
        )
    };
    let start = bytes
        .len()
        .checked_sub(JOB_CONFIG_ENVIRONMENT_TAG.len() + 16)
        .ok_or_else(invalid)?;
    if &bytes[start..start + JOB_CONFIG_ENVIRONMENT_TAG.len()] != JOB_CONFIG_ENVIRONMENT_TAG {
        return Err(invalid());
    }
    let values = &bytes[start + JOB_CONFIG_ENVIRONMENT_TAG.len()..];
    let budget = u64::from_be_bytes(values[..8].try_into().map_err(|_| invalid())?);
    let hours = u64::from_be_bytes(values[8..].try_into().map_err(|_| invalid())?);
    plakous_tools::JobConfig::new(budget, hours).map_err(|_| invalid())
}

pub(super) fn live_job_config(tools: &ToolRuntime) -> Result<plakous_tools::JobConfig, AgentError> {
    let limits = tools.environment().config().limits();
    plakous_tools::JobConfig::new(
        limits.foreground_seconds(),
        limits.session_duration().as_secs() / 3600,
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "live job environment is invalid",
        )
    })
}

pub(super) fn stable_workspace_environment_bytes(
    workspace: &WorkspacePolicy,
) -> Result<Vec<u8>, AgentError> {
    let root = workspace.root().as_os_str().as_encoded_bytes();
    let cwd = workspace.cwd().as_os_str().as_encoded_bytes();
    let root_len = u64::try_from(root.len()).map_err(|_| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "workspace root identity exceeds the runtime bound",
        )
    })?;
    let cwd_len = u64::try_from(cwd.len()).map_err(|_| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "workspace current-directory identity exceeds the runtime bound",
        )
    })?;
    let mut bytes = Vec::with_capacity(
        64_usize
            .saturating_add(root.len())
            .saturating_add(cwd.len()),
    );
    bytes.extend_from_slice(b"plakous-agent/workspace-environment-v1\0");
    bytes.extend_from_slice(std::env::consts::OS.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(b"host-path-policy-only\0");
    bytes.extend_from_slice(&root_len.to_be_bytes());
    bytes.extend_from_slice(root);
    bytes.extend_from_slice(&cwd_len.to_be_bytes());
    bytes.extend_from_slice(cwd);
    Ok(bytes)
}

pub(super) fn harness_plan_component(
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
    clippy::too_many_arguments,
    reason = "the checkpoint binding consumes the complete live reconstruction authority tuple"
)]
pub(super) fn runtime_manifest_binding_and_tools(
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
pub(super) fn initial_memory_checkpoint(
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
        durability,
    )
    .map_err(runtime_manifest_error)?;
    RuntimeCheckpoint::new(manifest).map_err(runtime_manifest_error)
}

#[cfg(test)]
mod prefix_mismatch_tests {
    use super::*;

    #[test]
    fn an_empty_prefix_drift_keeps_the_plain_checkpoint_mismatch() {
        use plakous_model::{StableRequestPrefixDrift, StableRequestPrefixField};
        assert_eq!(
            checkpoint_prefix_mismatch(StableRequestPrefixDrift::default()),
            "builder semantics disagree with the durable checkpoint"
        );
        assert_eq!(
            checkpoint_prefix_mismatch(
                StableRequestPrefixDrift::default()
                    .with(StableRequestPrefixField::Instructions)
                    .with(StableRequestPrefixField::ToolChoice)
            ),
            "builder semantics disagree with the durable checkpoint \
             (stable-prefix fields: instructions,tool-choice)"
        );
    }
}

#[cfg(test)]
mod job_config_tests {
    use super::*;

    #[test]
    fn composition_environment_reader_is_length_framed_and_fail_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let workspace = WorkspacePolicy::host(directory.path(), ".")?;
        let mut bytes = stable_workspace_environment_bytes(&workspace)?;
        let workspace_len = bytes.len();
        append_job_config(&mut bytes, plakous_tools::JobConfig::default());
        assert!(stored_composition_artifact(&bytes)?.is_none());
        bytes.truncate(workspace_len);
        bytes.extend_from_slice(b"\0plakous-agent/local-composition-attachment-v2\0");
        let identity =
            Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[b"fixture"])?.identity_bytes();
        for _ in 0..9 {
            bytes.extend_from_slice(&identity);
        }
        bytes.extend_from_slice(&10_000_u64.to_be_bytes());
        bytes.extend_from_slice(&10_000_u64.to_be_bytes());
        let host_length_offset = bytes.len();
        let host = b"rquickjs-host/fixture\0plakous-agent/job-limits-v1\0";
        bytes.extend_from_slice(&u64::try_from(host.len())?.to_be_bytes());
        bytes.extend_from_slice(host);
        append_job_config(&mut bytes, plakous_tools::JobConfig::default());
        let saved = stored_composition_artifact(&bytes)?.ok_or("missing artifact")?;
        assert_eq!(saved.host, host);
        assert_eq!(saved.artifact_identity, identity);
        assert_eq!(saved.engine_contract_identity, identity);
        for end in 0..bytes.len() {
            assert!(stored_composition_artifact(&bytes[..end]).is_err());
        }
        bytes[host_length_offset..host_length_offset + 8].copy_from_slice(&u64::MAX.to_be_bytes());
        assert!(stored_composition_artifact(&bytes).is_err());
        Ok(())
    }

    #[test]
    fn stored_job_config_refuses_missing_or_invalid_bounds() -> Result<(), AgentError> {
        let mut valid = b"environment identity".to_vec();
        append_job_config(&mut valid, plakous_tools::JobConfig::default());
        assert_eq!(
            stored_job_config(&valid)?,
            plakous_tools::JobConfig::default(),
        );
        let mut out_of_range = valid.clone();
        let budget_start = out_of_range.len() - 16;
        out_of_range[budget_start..budget_start + 8].copy_from_slice(&0_u64.to_be_bytes());
        let mut wrong_tag = valid.clone();
        wrong_tag[budget_start - 1] ^= 1;
        for invalid in [
            Vec::new(),
            b"environment identity".to_vec(),
            valid[..valid.len() - 1].to_vec(),
            out_of_range,
            wrong_tag,
        ] {
            assert_eq!(
                stored_job_config(&invalid).err().map(|error| error.kind()),
                Some(AgentErrorKind::IncompatibleSession),
            );
        }
        Ok(())
    }
}
