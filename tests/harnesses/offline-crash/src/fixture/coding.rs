use super::{
    AgentBuilder, AgentRequestPolicy, AlwaysAllowPolicy, Arc, AtomicU64, BoundaryController,
    CODE_MODE_EXEC_NAME, CODE_MODE_WAIT_NAME, CachePolicy, CompiledProviderConfiguration,
    ConfiguredLanguageModel, ConfiguredTool, ContextAdmissionRequirement, CredentialSelectionId,
    Digest, DigestDomain, EffectiveProviderConfiguration, FamilyPolicyEnvelope, FinishReason,
    FixedClock, GenerationLimits, HarnessResult, InMemoryPlanState, LOOPBACK_PROTOCOL,
    LanguageModel, LocalDurabilityDisposition, LoopbackModel, LoopbackScenario,
    ModelAdapterProfileId, ModelErrorKind, ModelImplementationIdentity, NamespaceId, NonZeroU32,
    OutputBoundDisposition, OutputPolicy, ParallelToolDisposition, Path, PlanStateIdentity,
    ProcessSessionTools, ProviderAuthorityClassId, ProviderBehaviorConfiguration,
    ProviderProfileRevisionId, ProviderRouteSelection, ProviderSemanticEvidence,
    ProviderStorageDisposition, ReasoningEffortDisposition, ReasoningIncludeDisposition,
    ReasoningPolicy, ReasoningSummaryDisposition, ReplayDisposition, ResolvedModelDescriptor,
    ResolvedProviderAuthorityBinding, ResponseStateDisposition, RetentionDisposition,
    RuntimeModelAuthority, SEMANTIC_COMPACTION_MAX_OUTPUT_TOKENS, ScriptPlan, ScriptedOperation,
    SelectedProviderConfiguration, SessionRuntimeBinding, SystemIdGenerator, ToolChoice,
    ToolConcurrency, ToolDefinition, ToolEffect, ToolExecutionDisposition, ToolInputContract,
    ToolName, ToolOutputContract, ToolRegistration, ToolRegistryBuilder, WorkspacePolicy,
    authority_bindings, descriptor, descriptor_over_protocol, descriptor_with_projected_input,
    fixture_time, json, script_descriptor, scripted_agent_config, scripted_code_mode_descriptor,
    scripted_runtime_binding_with_cursor_inner, scripted_tool_policy,
    scripted_topology_runtime_binding_inner,
};

/// [`super::scripted_code_mode_runtime_binding`] whose nested catalog also lists a
/// host's bound nested-eligible tool `extensions` (a template's MCP tools),
/// in order after the core tools, as the product's Code Mode catalogs do.
///
/// # Errors
///
/// Returns an error when the registry, projection target, profile, sidecar
/// artifact or workspace fixture is invalid, or an extension is not
/// nested-eligible or collides with a core tool.
#[allow(
    clippy::needless_pass_by_value,
    reason = "the caller hands the script's ownership to the binding it builds"
)]
pub fn scripted_code_mode_runtime_binding_with_extensions(
    workspace: &Path,
    controller: BoundaryController,
    script: Arc<ScriptPlan>,
    cursor: Arc<AtomicU64>,
    sidecar: &Path,
    telemetry: Option<Arc<plakous::agent::TelemetryEmitter>>,
    extensions: Vec<ToolRegistration>,
) -> HarnessResult<SessionRuntimeBinding> {
    scripted_code_mode_runtime_binding_inner(
        workspace,
        controller,
        &script,
        ScriptedOperation::Turn(cursor),
        sidecar,
        telemetry,
        extensions,
    )
}

fn scripted_code_mode_runtime_binding_inner(
    workspace: &Path,
    controller: BoundaryController,
    script: &Arc<ScriptPlan>,
    operation: ScriptedOperation,
    sidecar: &Path,
    telemetry: Option<Arc<plakous::agent::TelemetryEmitter>>,
    extensions: Vec<ToolRegistration>,
) -> HarnessResult<SessionRuntimeBinding> {
    let account = b"offline-scripted-code-mode-account";
    let descriptor = scripted_code_mode_descriptor(account, script.maximum_projected_input)?;
    let model = operation.model(controller, account, descriptor.clone(), script)?;

    // The Codex minimal catalog plus the two outer markers, which is exactly
    // what `CodexMinimalLocalCodeModeV1::compile` requires to be present.
    //
    // **Not `standard_coding_packs_in_memory`.** That helper mints its plan
    // state with `InMemoryPlanState::new()`, whose authority is
    // `memory.plan.<pid>.<counter>` (`tool-plan/src/lib.rs`). The harness plan
    // binds that authority, so a session created in one process compiles a
    // different plan from the one that reopens it in another, and revalidation
    // refuses with "builder harness plan disagrees with durable semantics". A
    // spawned-binary harness reopens across processes by definition, so the
    // identity has to be stable. Everything else here is what that helper
    // passes, `posix_sh` included.
    let plan_state = Arc::new(InMemoryPlanState::with_identity(PlanStateIdentity::new(
        "offline.scripted.code-mode.plan-state",
        1,
    )?));
    let mut registry = ToolRegistryBuilder::default();
    plakous::tools::register_packs(
        &plakous::tools::standard_coding_packs(
            plan_state,
            Arc::new(ProcessSessionTools::posix_sh()?),
        ),
        &mut registry,
    )?;
    let extension_names = extensions
        .iter()
        .map(|extension| extension.name().clone())
        .collect::<Vec<_>>();
    for extension in extensions {
        registry.register(extension)?;
    }
    if script.js_ptc {
        plakous::tools::ProductJavaScriptCompositionV1::register_markers_into(&mut registry)?;
    } else {
        plakous::tools::CodexMinimalLocalCodeModeV1::register_markers_into(&mut registry)?;
    }
    let registry = registry.freeze();

    // The projection target is *derived from the compiled configuration*, the
    // same way the qualification check derives the one it compares against.
    // Inventing it by hand produces a descriptor the check rejects even when
    // the target is enrolled.
    //
    // The configuration owns **exactly** the two outer markers, not the whole
    // registry, because that is what the enrolment gate requires of a Code Mode
    // provider configuration (`codex_code_mode.rs`, "does not own the exact
    // local exec/wait markers"). The nested catalog is reached through the
    // registry, which the agent is still built over; exposing it here as well
    // is what made this refuse.
    let marker_names = if script.js_ptc {
        ["exec_js", "wait_js"]
    } else {
        [CODE_MODE_EXEC_NAME, CODE_MODE_WAIT_NAME]
    };
    let markers = marker_names
        .into_iter()
        .map(|name| {
            registry
                .definition(&ToolName::new(name)?)
                .cloned()
                .ok_or_else(|| format!("Code Mode marker {name} is not registered").into())
        })
        .collect::<HarnessResult<Vec<ToolDefinition>>>()?;
    let configured = configured_model_with_tools(
        Arc::clone(&model),
        &descriptor,
        markers,
        account,
        None,
        plakous::model::ToolPresentationMode::LocalCompositionOnly,
    )?;
    let attachment = scripted_local_composition_attachment(
        sidecar,
        script.js_ptc,
        &registry,
        &configured,
        &extension_names,
    )?;

    let workspace = Arc::new(WorkspacePolicy::host(workspace, ".")?);
    // A composition deployment exposes its nested catalog through the
    // evaluator, never to the provider, so the harness plan's exposure strategy
    // is `None` and the derived exposure map is empty. The default plan exposes
    // the whole registry, which the agent then finds disagrees with a
    // configuration that owns only the two markers. This is the same call
    // `tests/fixtures/facade-consumer/src/embedded.rs` makes.
    let plan = plakous::construction::compile_local_composition_plan(
        plakous::agent::HarnessProfileId::new("offline.scripted.code-mode")?,
        &model,
        configured.configuration(),
        &registry,
        &workspace,
    )?;
    let builder = AgentBuilder::new(configured, registry, workspace)
        .harness_plan(plan)
        .config(scripted_agent_config(script))
        .tool_policy(scripted_tool_policy(script))
        .identity_sources(
            Arc::new(FixedClock::new(fixture_time()?)),
            Arc::new(SystemIdGenerator),
        );
    let builder = if script.scheduled_compaction {
        builder.with_scheduled_compaction_enabled(true)
    } else {
        builder
    };
    let builder = if let Some(telemetry) = telemetry {
        builder.telemetry(telemetry)
    } else {
        builder
    };
    Ok(SessionRuntimeBinding::new(
        plakous::agent::attach_local_composition(builder, Some(attachment))?,
    ))
}

fn scripted_local_composition_attachment(
    sidecar: &Path,
    js_ptc: bool,
    registry: &plakous::tools::FrozenToolRegistry,
    configured: &ConfiguredLanguageModel,
    extensions: &[ToolName],
) -> HarnessResult<plakous::agent::LocalCompositionAttachment> {
    let effective = configured.configuration().effective();
    let model_facts = effective.descriptor();
    let family = model_facts
        .model_family()
        .ok_or("scripted Code Mode descriptor has no model family")?;
    let target = plakous::tools::ProviderModelProjectionIdentityV2::new(
        model_facts.integration_class(),
        model_facts.protocol().as_str(),
        model_facts.route().as_str(),
        model_facts.profile().as_str(),
        family.as_str(),
        model_facts.requested_model().as_str(),
        effective.effective_model().as_str(),
        configured.configuration().effective_identity(),
        plakous::tools::PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    let artifact = plakous::agent::LocalCompositionEngineArtifact::quickjs_worker_v1(
        std::fs::canonicalize(sidecar)?,
    )?;
    let attachment = if js_ptc {
        let compiled = plakous::tools::ProductJavaScriptCompositionV1::compile_for_kind(
            plakous::tools::JobConfig::default(),
            registry.clone(),
            target,
            plakous::tools::CodingPresentation::Codex,
            [
                ToolName::new("exec_command")?,
                ToolName::new("write_stdin")?,
            ]
            .into_iter()
            .chain(extensions.iter().cloned()),
        )?;
        assert!(compiled.outer_projection().is_for_registry(registry));
        let profile = plakous::agent::LocalCompositionProfile::cell(&(&compiled).into())?;
        let identity = Digest::sha256(
            DigestDomain::ExecutionEnvironment,
            1,
            &[b"offline.scripted.js-ptc"],
        )?;
        let evidence = plakous::agent::ProductExtensionEvidence::cell(
            &profile,
            &artifact,
            identity,
            identity,
            configured.configuration(),
        )?;
        plakous::agent::LocalCompositionAttachment::builder(profile)
            .engine_artifact(artifact)
            .product_extension_evidence(evidence)
            .coding_presentation_kind(plakous::tools::CodingPresentation::Codex)
            .build()?
    } else {
        let compiled = plakous::tools::CodexMinimalLocalCodeModeV1::compile_with_extensions(
            plakous::tools::JobConfig::default(),
            registry.clone(),
            target,
            extensions,
        )?;
        assert!(compiled.outer_projection().is_for_registry(registry));
        let profile = plakous::agent::LocalCompositionProfile::cell(&(&compiled).into())?;
        plakous::agent::LocalCompositionAttachment::builder(profile)
            .engine_artifact(artifact)
            .build()?
    };

    Ok(attachment)
}

impl ScriptedOperation {
    pub(super) fn model(
        self,
        controller: BoundaryController,
        account: &[u8],
        descriptor: ResolvedModelDescriptor,
        script: &Arc<ScriptPlan>,
    ) -> HarnessResult<Arc<dyn LanguageModel>> {
        let mut model = LoopbackModel::with_descriptor(controller, account, false, descriptor)?;
        match self {
            Self::ArtifactTurn(cursor) => {
                model.scenario = LoopbackScenario::Scripted(Arc::clone(script));
                model = model.with_shared_round(cursor);
                model.seal_final_artifact = true;
            }
            Self::Turn(cursor) => {
                model.scenario = LoopbackScenario::Scripted(Arc::clone(script));
                model = model.with_shared_round(cursor);
            }
            Self::Compact(finish) => {
                model.scenario = LoopbackScenario::ScriptedSummary(Arc::clone(script), finish);
            }
        }
        if let Some(namespace) = script.identity_namespace {
            // Same 48-bit width as the account-derived namespace. Wider values
            // are refused by `ScriptPlan::validate`; mask here too so a caller
            // that skips validation cannot shift into a debug overflow.
            model.identity_namespace = namespace & 0x0000_0000_0000_0000_0000_ffff_ffff_ffff;
        }
        Ok(Arc::new(model))
    }
}

/// Builds an exact twin of the scripted source binding for summary requests.
///
/// Source tools, configuration identities and authority are preserved; only
/// the model scenario and compaction generation limit change. `extensions`
/// are the source binding's root extensions; a topology twin carries
/// `tree_tools` instead, as its source does.
///
/// # Errors
///
/// Returns an error when the source configuration, topology registrations,
/// Code Mode sidecar or compaction output policy cannot be built.
pub fn scripted_compaction_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
    script: &Arc<ScriptPlan>,
    tree_tools: Option<Vec<ToolRegistration>>,
    sidecar: Option<&Path>,
    extensions: Vec<ToolRegistration>,
) -> HarnessResult<SessionRuntimeBinding> {
    scripted_compaction_runtime_binding_with_finish(
        workspace,
        controller,
        script,
        tree_tools,
        sidecar,
        extensions,
        FinishReason::Stop,
    )
}

/// Builds the scripted summary twin with an explicit terminal reason.
///
/// # Errors
///
/// Returns an error when the scripted source binding or summary limit is invalid.
pub fn scripted_compaction_runtime_binding_with_finish(
    workspace: &Path,
    controller: BoundaryController,
    script: &Arc<ScriptPlan>,
    tree_tools: Option<Vec<ToolRegistration>>,
    sidecar: Option<&Path>,
    extensions: Vec<ToolRegistration>,
    finish: FinishReason,
) -> HarnessResult<SessionRuntimeBinding> {
    let runtime = if let Some(tree_tools) = tree_tools {
        scripted_topology_runtime_binding_inner(
            workspace,
            controller,
            script,
            ScriptedOperation::Compact(finish),
            tree_tools,
            Arc::new(FixedClock::new(fixture_time()?)),
        )?
    } else if script.code_mode {
        scripted_code_mode_runtime_binding_inner(
            workspace,
            controller,
            script,
            ScriptedOperation::Compact(finish),
            sidecar.ok_or("scripted Code Mode compaction needs its source sidecar")?,
            None,
            extensions,
        )?
    } else {
        scripted_runtime_binding_with_cursor_inner(
            workspace,
            controller,
            script,
            ScriptedOperation::Compact(finish),
            extensions,
        )?
    };
    Ok(runtime.with_compaction_output_limit()?)
}

/// Builds the no-tools loopback summary binding used by compaction evidence.
///
/// # Errors
///
/// Returns an error when the summary model or workspace fixture is invalid.
pub fn compaction_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
) -> HarnessResult<SessionRuntimeBinding> {
    compaction_runtime_binding_with_projected_input(workspace, controller, 10_000_000)
}

/// Builds the no-tools loopback summary binding with an exact input ceiling.
///
/// # Errors
///
/// Returns an error when the summary model or workspace fixture is invalid.
pub fn compaction_runtime_binding_with_projected_input(
    workspace: &Path,
    controller: BoundaryController,
    maximum_projected_input: u64,
) -> HarnessResult<SessionRuntimeBinding> {
    compaction_runtime_binding_with_projected_input_inner(
        workspace,
        controller,
        maximum_projected_input,
        None,
    )
}

/// Builds the bounded no-tools summary binding with a caller-separated response namespace.
///
/// # Errors
///
/// Returns an error when the summary model or workspace fixture is invalid.
pub fn compaction_runtime_binding_with_projected_input_and_identity_namespace(
    workspace: &Path,
    controller: BoundaryController,
    maximum_projected_input: u64,
    identity_namespace: u128,
) -> HarnessResult<SessionRuntimeBinding> {
    compaction_runtime_binding_with_projected_input_inner(
        workspace,
        controller,
        maximum_projected_input,
        Some(identity_namespace),
    )
}

/// Builds a text-only fixture with a streamed preview and independent canonical text.
/// The selected provider-outcome boundary gates completion after the preview.
///
/// # Errors
/// Returns an error when model or workspace authority cannot be constructed.
pub fn text_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
    prefix: &'static str,
    terminal: &'static str,
) -> HarnessResult<SessionRuntimeBinding> {
    text_runtime_binding_with_finish(workspace, controller, prefix, terminal, FinishReason::Stop)
}

/// Builds a text fixture with an explicit canonical terminal reason.
///
/// # Errors
/// Returns an error when model or workspace authority cannot be constructed.
pub fn text_runtime_binding_with_finish(
    workspace: &Path,
    controller: BoundaryController,
    prefix: &'static str,
    terminal: &'static str,
    finish: FinishReason,
) -> HarnessResult<SessionRuntimeBinding> {
    text_scenario_runtime_binding(
        workspace,
        controller,
        LoopbackScenario::Text {
            prefix,
            terminal,
            finish,
        },
    )
}

/// Builds a text-only fixture preserving multiple canonical blocks.
///
/// # Errors
/// Returns an error when model or workspace authority cannot be constructed.
pub fn text_blocks_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
    texts: &'static [&'static str],
) -> HarnessResult<SessionRuntimeBinding> {
    text_scenario_runtime_binding(workspace, controller, LoopbackScenario::TextBlocks(texts))
}

/// Builds a text-only fixture over an explicit account and protocol, for
/// cross-provider history tests without credentials or inference.
///
/// # Errors
/// Returns invalid target or workspace construction inputs.
pub fn text_runtime_binding_for_target(
    workspace: &Path,
    controller: BoundaryController,
    account: &[u8],
    protocol: &str,
) -> HarnessResult<SessionRuntimeBinding> {
    text_scenario_runtime_binding_for_target(
        workspace,
        controller,
        LoopbackScenario::Text {
            prefix: "carry",
            terminal: "carry",
            finish: FinishReason::Stop,
        },
        account,
        protocol,
    )
}

fn text_scenario_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
    scenario: LoopbackScenario,
) -> HarnessResult<SessionRuntimeBinding> {
    text_scenario_runtime_binding_for_target(
        workspace,
        controller,
        scenario,
        b"offline-loopback-account",
        LOOPBACK_PROTOCOL,
    )
}

fn text_scenario_runtime_binding_for_target(
    workspace: &Path,
    controller: BoundaryController,
    scenario: LoopbackScenario,
    account: &[u8],
    protocol: &str,
) -> HarnessResult<SessionRuntimeBinding> {
    let descriptor = descriptor_over_protocol(account, 10_000_000, protocol, false)?;
    let mut model = LoopbackModel::with_descriptor(controller, account, true, descriptor.clone())?;
    model.scenario = scenario;
    let configured = configured_model_with_tools(
        Arc::new(model),
        &descriptor,
        Vec::new(),
        account,
        NonZeroU32::new(1_024),
        plakous::model::ToolPresentationMode::Direct,
    )?;
    let builder = AgentBuilder::new(
        configured,
        ToolRegistryBuilder::default().freeze(),
        Arc::new(WorkspacePolicy::host(workspace, ".")?),
    )
    .request_policy(AgentRequestPolicy::new(
        GenerationLimits::new(NonZeroU32::new(1_024), Vec::new())?,
        ReasoningPolicy::Disabled,
    ))
    .identity_sources(
        Arc::new(FixedClock::new(fixture_time()?)),
        Arc::new(SystemIdGenerator),
    );
    Ok(SessionRuntimeBinding::new(builder))
}

/// Builds a no-tools runtime whose dispatched provider call returns an exact
/// classified failure for terminal-authority integration tests.
///
/// # Errors
///
/// Returns an error if the deterministic runtime fixture cannot be constructed.
pub fn terminal_failure_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
    kind: ModelErrorKind,
) -> HarnessResult<SessionRuntimeBinding> {
    let account = b"offline-loopback-account";
    let descriptor = descriptor(account)?;
    let mut model = LoopbackModel::with_descriptor(controller, account, true, descriptor.clone())?;
    model.scenario = LoopbackScenario::TerminalFailure(kind);
    let configured = configured_model_with_tools(
        Arc::new(model),
        &descriptor,
        Vec::new(),
        account,
        NonZeroU32::new(1_024),
        plakous::model::ToolPresentationMode::Direct,
    )?;
    let builder = AgentBuilder::new(
        configured,
        ToolRegistryBuilder::default().freeze(),
        Arc::new(WorkspacePolicy::host(workspace, ".")?),
    )
    .request_policy(AgentRequestPolicy::new(
        GenerationLimits::new(NonZeroU32::new(1_024), Vec::new())?,
        ReasoningPolicy::Disabled,
    ))
    .identity_sources(
        Arc::new(FixedClock::new(fixture_time()?)),
        Arc::new(SystemIdGenerator),
    );
    Ok(SessionRuntimeBinding::new(builder))
}

/// [`terminal_failure_runtime_binding`] shaped like a product coding session:
/// the standard coding packs under an exact V2 Direct projection, so a
/// product host's projection-reconstruction gate admits its reopen. The model
/// still fails its first turn with `kind` and never calls a tool.
///
/// # Errors
///
/// Returns an error when the model, the coding packs, or the projection is invalid.
pub fn projected_terminal_failure_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
    kind: ModelErrorKind,
) -> HarnessResult<SessionRuntimeBinding> {
    let account = b"offline-loopback-account";
    let descriptor = descriptor(account)?;
    let mut model = LoopbackModel::with_descriptor(controller, account, true, descriptor.clone())?;
    model.scenario = LoopbackScenario::TerminalFailure(kind);
    let builder = projected_coding_builder(
        workspace,
        Arc::new(model),
        &descriptor,
        account,
        plakous::tools::CodingPresentation::Codex,
    )?;
    Ok(SessionRuntimeBinding::new(builder))
}

/// The scripted model over the standard coding packs, presented in pi's
/// vocabulary under an exact V2 Direct projection.
///
/// pi exposes one canonical patch tool, `apply_patch`, as two names, `edit`
/// and `write`, so a script that calls both commits two calls with one
/// canonical name and two exposed names.
///
/// # Errors
///
/// Returns an error when the scripted model, the coding packs, or the
/// projection is invalid.
pub fn scripted_pi_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
    script: &Arc<ScriptPlan>,
) -> HarnessResult<SessionRuntimeBinding> {
    let account = b"offline-loopback-account";
    let descriptor = script_descriptor(account, script)?;
    let model = ScriptedOperation::Turn(Arc::new(AtomicU64::new(1))).model(
        controller,
        account,
        descriptor.clone(),
        script,
    )?;
    let builder = projected_coding_builder(
        workspace,
        model,
        &descriptor,
        account,
        plakous::tools::CodingPresentation::Pi,
    )?
    .config(scripted_agent_config(script))
    .tool_policy(scripted_tool_policy(script));
    Ok(SessionRuntimeBinding::new(builder))
}

/// The standard coding packs under `presentation`'s exact V2 Direct
/// projection, so a product host's projection-reconstruction gate admits a
/// reopen.
fn projected_coding_builder(
    workspace: &Path,
    model: Arc<dyn LanguageModel>,
    descriptor: &ResolvedModelDescriptor,
    account: &[u8],
    presentation: plakous::tools::CodingPresentation,
) -> HarnessResult<AgentBuilder> {
    use plakous::tools::CodingPresentationExt as _;
    // A fixed plan-state identity, so a reopen's fresh binding names the same
    // executor registrations the create froze.
    let plan_state = Arc::new(InMemoryPlanState::with_identity(PlanStateIdentity::new(
        "offline.projected.plan-state",
        1,
    )?));
    let mut registry = ToolRegistryBuilder::default();
    plakous::tools::register_packs(
        &plakous::tools::standard_coding_packs(
            plan_state,
            Arc::new(ProcessSessionTools::posix_sh()?),
        ),
        &mut registry,
    )?;
    let registry = registry.freeze();
    // The agent's default exposure lists the registry in canonical-name
    // order, and the configured tools must match it exactly.
    let mut definitions = registry.definitions().to_vec();
    definitions.sort_by(|left, right| left.name().as_str().cmp(right.name().as_str()));
    let configured = configured_model_with_tools(
        model,
        descriptor,
        definitions,
        account,
        None,
        plakous::model::ToolPresentationMode::Direct,
    )?;
    let effective = configured.configuration().effective();
    let facts = effective.descriptor();
    let family = facts
        .model_family()
        .ok_or("projected fixture descriptor has no model family")?;
    let target = plakous::tools::ProviderModelProjectionIdentityV2::new(
        facts.integration_class(),
        facts.protocol().as_str(),
        facts.route().as_str(),
        facts.profile().as_str(),
        family.as_str(),
        facts.requested_model().as_str(),
        effective.effective_model().as_str(),
        configured.configuration().effective_identity(),
        plakous::tools::PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    let projection = presentation.projection_map(
        plakous::tools::JobConfig::default(),
        registry.clone(),
        target,
    )?;
    Ok(AgentBuilder::new(
        configured,
        registry,
        Arc::new(WorkspacePolicy::host(workspace, ".")?),
    )
    .tool_projection(projection)
    .tool_policy(Arc::new(AlwaysAllowPolicy))
    .identity_sources(
        Arc::new(FixedClock::new(fixture_time()?)),
        Arc::new(SystemIdGenerator),
    ))
}

fn compaction_runtime_binding_with_projected_input_inner(
    workspace: &Path,
    controller: BoundaryController,
    maximum_projected_input: u64,
    identity_namespace: Option<u128>,
) -> HarnessResult<SessionRuntimeBinding> {
    let account = b"offline-loopback-account";
    let descriptor = descriptor_with_projected_input(account, maximum_projected_input)?;
    let mut model = LoopbackModel::with_descriptor(controller, account, true, descriptor.clone())?;
    if let Some(identity_namespace) = identity_namespace {
        model.identity_namespace = identity_namespace;
    }
    let model: Arc<dyn LanguageModel> = Arc::new(model);
    let configured = configured_model(
        Arc::clone(&model),
        &descriptor,
        ToolDefinition::new(
            ToolName::new("unused_compaction_tool")?,
            "1.0.0",
            "Never exposed during compaction.",
            ToolInputContract::structured(json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }))?,
            ToolOutputContract::portable_model_content(),
            ToolConcurrency::Serial,
            ToolEffect::ReadOnly,
        )?,
        account,
        Some(SEMANTIC_COMPACTION_MAX_OUTPUT_TOKENS),
    )?;
    let registry = ToolRegistryBuilder::default().freeze();
    let workspace = Arc::new(WorkspacePolicy::host(workspace, ".")?);
    let builder = AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(Some(SEMANTIC_COMPACTION_MAX_OUTPUT_TOKENS), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .identity_sources(
            Arc::new(FixedClock::new(fixture_time()?)),
            Arc::new(SystemIdGenerator),
        );
    Ok(SessionRuntimeBinding::new(builder))
}

pub(super) fn configured_model(
    model: Arc<dyn LanguageModel>,
    descriptor: &ResolvedModelDescriptor,
    tool: ToolDefinition,
    account: &[u8],
    output_bound: Option<NonZeroU32>,
) -> HarnessResult<ConfiguredLanguageModel> {
    configured_model_with_tools(
        model,
        descriptor,
        vec![tool],
        account,
        output_bound,
        plakous::model::ToolPresentationMode::Direct,
    )
}

pub(super) fn configured_model_with_tools(
    model: Arc<dyn LanguageModel>,
    descriptor: &ResolvedModelDescriptor,
    tools: Vec<ToolDefinition>,
    account: &[u8],
    output_bound: Option<NonZeroU32>,
    tool_mode: plakous::model::ToolPresentationMode,
) -> HarnessResult<ConfiguredLanguageModel> {
    let tools = tools
        .into_iter()
        .map(|tool| {
            ConfiguredTool::new(
                tool,
                ToolExecutionDisposition::Local,
                plakous::model::ToolCallerDisposition::DirectOnly,
            )
        })
        .collect();
    configured_model_with_configured_tools(
        model,
        descriptor,
        tools,
        account,
        output_bound,
        tool_mode,
    )
}

/// Direct configured tools for every definition in `registry`, in registry
/// order, each carrying its registration's owner as the product's Direct
/// extensions do.
pub(super) fn direct_configured_tools(
    registry: &plakous::tools::FrozenToolRegistry,
) -> HarnessResult<Vec<ConfiguredTool>> {
    registry
        .definitions()
        .iter()
        .map(|definition| {
            let tool = ConfiguredTool::new(
                definition.clone(),
                ToolExecutionDisposition::Local,
                plakous::model::ToolCallerDisposition::DirectOnly,
            );
            match registry
                .identity(definition.name())
                .map(plakous::tools::RegisteredToolIdentity::owner)
            {
                Some(plakous::tools::ToolOwnerClass::Mcp(server)) => {
                    Ok(tool.with_mcp_owner(server.canonical().clone())?)
                }
                _ => Ok(tool),
            }
        })
        .collect()
}

#[allow(
    clippy::too_many_lines,
    reason = "complete crash-evidence fixture construction"
)]
pub(super) fn configured_model_with_configured_tools(
    model: Arc<dyn LanguageModel>,
    descriptor: &ResolvedModelDescriptor,
    tools: Vec<ConfiguredTool>,
    account: &[u8],
    output_bound: Option<NonZeroU32>,
    tool_mode: plakous::model::ToolPresentationMode,
) -> HarnessResult<ConfiguredLanguageModel> {
    let (account_binding, credential_binding) = authority_bindings(account)?;
    let authority = RuntimeModelAuthority::new(
        descriptor,
        account_binding,
        credential_binding,
        1,
        fixture_time()?,
        None,
    )?;
    let behavior = ProviderBehaviorConfiguration {
        processing_tier: plakous::model::ProcessingTierDisposition::Standard,
        tool_mode,
        reasoning_effort: ReasoningEffortDisposition::Disabled,
        reasoning_summary: ReasoningSummaryDisposition::Omit,
        reasoning_include: ReasoningIncludeDisposition::Omit,
        replay: ReplayDisposition::Disabled,
        cache: CachePolicy::Disabled,
        tools: if output_bound.is_some() {
            Vec::new()
        } else {
            tools
        },
        hosted_tools: Vec::new(),
        tool_choice: if output_bound.is_some() {
            ToolChoice::None
        } else {
            ToolChoice::Auto
        },
        parallel_tools: ParallelToolDisposition::Disabled,
        output: OutputPolicy::default(),
        output_bound: output_bound.map_or(
            OutputBoundDisposition::NoRequestBound,
            OutputBoundDisposition::Enforced,
        ),
        storage: ProviderStorageDisposition::OmittedRouteContract,
        retention: RetentionDisposition::RouteContract {
            profile: descriptor.profile().clone(),
            revision: ProviderProfileRevisionId::new("offline-loopback-retention-v1")?,
        },
        response_state: ResponseStateDisposition::Stateless,
        context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
        local_durability: LocalDurabilityDisposition::DurableRequired,
    };
    let selected = SelectedProviderConfiguration::new(
        ProviderRouteSelection::new(
            descriptor.protocol().clone(),
            descriptor.route().clone(),
            descriptor.profile().clone(),
            ProviderProfileRevisionId::new("offline-loopback-route-v1")?,
            ModelAdapterProfileId::new("offline.loopback.auth")?,
            ProviderProfileRevisionId::new("offline-loopback-auth-v1")?,
            ProviderAuthorityClassId::new("offline-loopback-synthetic")?,
            CredentialSelectionId::new("offline-loopback-credential")?,
            descriptor.requested_model().clone(),
        ),
        behavior.clone(),
    )?;
    let stable = ResolvedProviderAuthorityBinding::new(
        descriptor,
        &authority,
        ProviderSemanticEvidence::new(1, b"offline-loopback-provider-semantics-v1".to_vec())?,
    )?;
    let family = FamilyPolicyEnvelope::new(
        descriptor
            .model_family()
            .cloned()
            .ok_or("offline loopback model family is missing")?,
        1,
        1,
        1,
        if output_bound.is_some() {
            b"offline-loopback-compaction-family-policy-v1".to_vec()
        } else {
            b"offline-loopback-family-policy-v1".to_vec()
        },
    )?;
    let effective = EffectiveProviderConfiguration::compile(
        &selected,
        descriptor.clone(),
        if descriptor.integration_class() == plakous::model::IntegrationClass::Standard {
            ModelImplementationIdentity::release_owned(
                &plakous_model::StandardIntegrationAuthority::release_owned(),
                descriptor,
                1,
            )?
        } else {
            ModelImplementationIdentity::custom(
                NamespaceId::new("offline.crash-harness.loopback-model")?,
                1,
                NamespaceId::new("offline.crash-harness.fixture")?,
            )?
        },
        stable,
        behavior,
        family,
    )?;
    let configuration = CompiledProviderConfiguration::new(selected, effective)?;
    if descriptor.integration_class() == plakous::model::IntegrationClass::Standard {
        Ok(ConfiguredLanguageModel::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            model,
            configuration,
            authority,
        )?)
    } else {
        Ok(ConfiguredLanguageModel::custom(
            model,
            configuration,
            authority,
        )?)
    }
}
