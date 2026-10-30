use super::{
    AgentBuilder, AlwaysAllowPolicy, Arc, AtomicU64, BTreeMap, BoundaryController, Capability,
    CapabilityConstraints, CapabilitySet, CapacityCertificate, CapacityRule, ContextContract,
    ContextEvidenceSource, CountCertificateKind, DecisionRequiredPolicy, Digest, DigestDomain,
    EvidenceFreshness, FactProvenance, FactSource, FeatureSupport, FixedClock, HarnessResult,
    InMemoryPlanState, LOOPBACK_PROTOCOL, LanguageModel, LoopbackEffect, LoopbackModel,
    LoopbackScenario, MeasurementCertificate, ModelAdapterProfileId, ModelFamilyId, ModelId,
    NamespaceId, Path, PlanStateIdentity, ProcessSessionTools, ProtocolId, RegisteredToolId,
    ResolvedModelDescriptor, RouteId, RuntimeModelAuthority, ScriptPlan, ScriptToolPolicy,
    ScriptedOperation, SessionRuntimeBinding, Support, SystemIdGenerator, ToolConcurrency,
    ToolDefinition, ToolEffect, ToolExecutorId, ToolExposure, ToolExposureMap, ToolInputContract,
    ToolName, ToolOutputContract, ToolPolicy, ToolRegistration, ToolRegistryBuilder,
    WorkspacePolicy, artifact_fixture_descriptor, configured_model,
    configured_model_with_configured_tools, configured_model_with_tools,
    descriptor_with_projected_input, direct_configured_tools, fixture_time, json,
    scripted_code_mode_runtime_binding_with_extensions,
};

pub(super) fn descriptor(account: &[u8]) -> HarnessResult<ResolvedModelDescriptor> {
    descriptor_with_projected_input(account, 10_000_000)
}

/// The loopback descriptor a script declares: its projected-input bound and,
/// when [`ScriptPlan::image_input`] is set, native image input.
pub(super) fn script_descriptor(
    account: &[u8],
    script: &ScriptPlan,
) -> HarnessResult<ResolvedModelDescriptor> {
    let mut descriptor = descriptor_over_protocol(
        account,
        script.maximum_projected_input,
        LOOPBACK_PROTOCOL,
        script.image_input,
    )?;
    if script.native_compaction {
        let capability = FeatureSupport::new(
            Support::Native,
            CapabilityConstraints::default(),
            FactProvenance::new(FactSource::TestFixture, fixture_time()?, None),
        );
        let native = BTreeMap::from([(NamespaceId::new("plakous.compaction")?, capability)]);
        descriptor = ResolvedModelDescriptor::new_custom(
            descriptor.protocol().clone(),
            descriptor.route().clone(),
            descriptor.requested_model().clone(),
            None,
            descriptor.model_family().cloned(),
            descriptor.profile().clone(),
            CapabilitySet::new(descriptor.capabilities().features().clone(), native),
        )
        .with_context_contract(
            descriptor
                .context_contract()
                .cloned()
                .ok_or("native fixture context contract")?,
        );
    }
    if script.omit_context_contract {
        return Ok(ResolvedModelDescriptor::new_custom(
            descriptor.protocol().clone(),
            descriptor.route().clone(),
            descriptor.requested_model().clone(),
            None,
            descriptor.model_family().cloned(),
            ModelAdapterProfileId::new("offline-loopback-adapter-v1")?,
            descriptor.capabilities().clone(),
        ));
    }
    Ok(descriptor)
}

/// [`descriptor_with_projected_input`] over a named protocol family.
///
/// The context contract names its own protocol family, and the descriptor's
/// admission compares the two (`crates/core/model/src/descriptor.rs`, the
/// `"prepared context contract"` drift). A descriptor that borrows another
/// family's contract is refused at admission, not at construction, so the two
/// are minted together here rather than substituted afterwards.
pub(super) fn descriptor_over_protocol(
    account: &[u8],
    maximum_projected_input: u64,
    protocol: &str,
    image_input: bool,
) -> HarnessResult<ResolvedModelDescriptor> {
    let provenance = FactProvenance::new(FactSource::TestFixture, fixture_time()?, None);
    let mut features = BTreeMap::from([
        (
            Capability::Streaming,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                provenance,
            ),
        ),
        (
            Capability::Tools,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                provenance,
            ),
        ),
    ]);
    if image_input {
        features.insert(
            Capability::ImageInput,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                provenance,
            ),
        );
    }
    let freshness = EvidenceFreshness::new(fixture_time()?, None)?;
    let (account_binding, credential_binding) = authority_bindings(account)?;
    let authority_binding =
        RuntimeModelAuthority::authority_class_for(account_binding, credential_binding)?;
    let declaration = Digest::sha256(
        DigestDomain::ModelBinding,
        1,
        &[format!(
            "offline loopback complete-projection upper bound: {maximum_projected_input} bytes"
        )
        .as_bytes()],
    )?;
    let contract = ContextContract::new(
        ProtocolId::new(protocol)?,
        "offline-loopback-accounting-v1",
        "offline-loopback-projection-v1",
        "offline-loopback-output-reservation-v1",
        CapacityCertificate::custom_envelope(
            "offline-loopback-capacity",
            "exact-v1",
            CapacityRule::ProviderReservedInput {
                reported_context_window: None,
                maximum_projected_input,
                auto_compact_input: None,
            },
            authority_binding,
            freshness,
            declaration,
            "offline-loopback-declaration-v1",
        )?,
        vec![MeasurementCertificate::new(
            "offline-loopback-byte-counter",
            "exact-v1",
            "complete-loopback-wire",
            "byte-tokenizer-v1",
            ContextEvidenceSource::CallerDeclaration,
            CountCertificateKind::CustomEnvelopeUpperBound { declaration },
            Some(authority_binding),
            freshness,
        )?],
    )?;
    Ok(ResolvedModelDescriptor::new_custom(
        ProtocolId::new(protocol)?,
        RouteId::new("offline.loopback")?,
        ModelId::new("offline-loopback-v1")?,
        None,
        Some(ModelFamilyId::new("offline.loopback.responses")?),
        ModelAdapterProfileId::new("offline-loopback-adapter-v1")?,
        CapabilitySet::new(features, BTreeMap::new()),
    )
    .with_context_contract(contract))
}

/// The scripted Code Mode descriptor, whose ids are the enrolled target.
///
/// Every field carries a `scripted` marker and matches the extra enrolment
/// `plakous-compose` adds under `scripted-code-mode-enrolment`, so this can
/// never be mistaken for a Codex model and never widens anything when that
/// feature is off.
pub(super) fn scripted_code_mode_descriptor(
    account: &[u8],
    maximum_projected_input: u64,
) -> HarnessResult<ResolvedModelDescriptor> {
    let base = descriptor_over_protocol(
        account,
        maximum_projected_input,
        "offline.scripted.code-mode.responses",
        false,
    )?;
    Ok(ResolvedModelDescriptor::new_custom(
        ProtocolId::new("offline.scripted.code-mode.responses")?,
        RouteId::new("offline.scripted.code-mode")?,
        ModelId::new("offline-scripted-code-mode-v1")?,
        None,
        Some(ModelFamilyId::new("offline.scripted.code-mode.responses")?),
        ModelAdapterProfileId::new("offline-scripted-code-mode-adapter-v1")?,
        base.capabilities().clone(),
    )
    .with_context_contract(
        base.context_contract()
            .cloned()
            .ok_or("scripted Code Mode descriptor lost its context contract")?,
    ))
}

pub(super) fn authority_bindings(account: &[u8]) -> HarnessResult<(Digest, Digest)> {
    Ok((
        Digest::sha256(DigestDomain::ModelBinding, 1, &[account])?,
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"offline-loopback-credential-authority"],
        )?,
    ))
}

/// Builds the default deterministic loopback runtime binding.
///
/// # Errors
///
/// Returns an error when the deterministic model, tool, or workspace fixture is invalid.
pub fn runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
) -> HarnessResult<SessionRuntimeBinding> {
    runtime_binding_for_account(workspace, controller, b"offline-loopback-account")
}

/// Builds the loopback runtime whose one tool reports a bounded execution
/// failure, so a driver can be scripted against a turn with a tool error.
///
/// # Errors
///
/// Returns an error when the deterministic model, tool, or workspace fixture is invalid.
pub fn failing_effect_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
) -> HarnessResult<SessionRuntimeBinding> {
    runtime_binding_inner(
        workspace,
        controller,
        b"offline-loopback-account",
        10_000_000,
        None,
        true,
    )
}

/// Builds a deterministic loopback runtime binding for an exact account identity.
///
/// # Errors
///
/// Returns an error when the account-bound model, tool, or workspace fixture is invalid.
pub fn runtime_binding_for_account(
    workspace: &Path,
    controller: BoundaryController,
    account: &[u8],
) -> HarnessResult<SessionRuntimeBinding> {
    runtime_binding_with_input(workspace, controller, account, 10_000_000, None)
}

/// Builds an account-bound loopback runtime with a distinct response-identity namespace.
///
/// The namespace affects only synthetic response entry/call identities. It is
/// intentionally outside provider/model semantic authority so tests can create
/// multiple runtime instances for one exact binding without inventing duplicate
/// provider response identities across forked history.
///
/// # Errors
///
/// Returns an error when the account-bound model, tool, or workspace fixture is invalid.
pub fn runtime_binding_for_account_with_identity_namespace(
    workspace: &Path,
    controller: BoundaryController,
    account: &[u8],
    identity_namespace: u128,
) -> HarnessResult<SessionRuntimeBinding> {
    runtime_binding_with_input(
        workspace,
        controller,
        account,
        10_000_000,
        Some(identity_namespace),
    )
}

/// Builds a loopback runtime whose admitted input ceiling is one token.
///
/// An empty conversation still overflows because the loopback wire projection
/// is larger than that ceiling, so inspect can report overflow without a paid turn.
///
/// # Errors
///
/// Returns an error when the deterministic model, tool, or workspace fixture is invalid.
pub fn overflow_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
) -> HarnessResult<SessionRuntimeBinding> {
    runtime_binding_with_input(workspace, controller, b"offline-loopback-account", 1, None)
}

/// Builds a deterministic canonical joined-process lifecycle whose first
/// observation is exactly 8,192 bytes from a 10,106-byte already-exited child.
///
/// This repository-only injected fixture proves the continuation handle,
/// follow-up drain, and quiescent terminal turn through the real executor.
///
/// # Errors
///
/// Returns an error when the exact model, projection, process, or workspace
/// authority cannot be constructed.
pub fn exited_child_process_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
) -> HarnessResult<SessionRuntimeBinding> {
    let account = b"offline-loopback-process-account";
    let model: Arc<dyn LanguageModel> = Arc::new(LoopbackModel::exited_child_continuation(
        controller, account,
    )?);
    let process = Arc::new(ProcessSessionTools::posix_sh()?);
    let plan_state = Arc::new(InMemoryPlanState::with_identity(PlanStateIdentity::new(
        "offline.process.fixture.plan-state",
        1,
    )?));
    let mut registry = ToolRegistryBuilder::default();
    plakous::tools::register_packs(
        &plakous::tools::standard_coding_packs(plan_state, process),
        &mut registry,
    )?;
    let registry = registry.freeze();
    let exposures = registry
        .definitions()
        .iter()
        .map(|definition| {
            let identity = registry
                .identity(definition.name())
                .cloned()
                .ok_or("process fixture registry identity is missing")?;
            ToolExposure::new(identity, definition.name().as_str().to_owned())
                .map_err(|_| "process fixture exposure is invalid".into())
        })
        .collect::<HarnessResult<Vec<_>>>()?;
    let exposure = ToolExposureMap::new(registry.clone(), exposures)?;
    let definitions = exposure.definitions().to_vec();
    let configured = configured_model_with_tools(
        Arc::clone(&model),
        &descriptor(account)?,
        definitions,
        account,
        None,
        plakous::model::ToolPresentationMode::Direct,
    )?;
    let workspace = Arc::new(WorkspacePolicy::host(workspace, ".")?);
    let builder = AgentBuilder::new(configured, registry, workspace)
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .identity_sources(
            Arc::new(FixedClock::new(fixture_time()?)),
            Arc::new(SystemIdGenerator),
        );
    Ok(SessionRuntimeBinding::new(builder))
}

/// Builds the loopback runtime whose one turn per round drives a tree:
/// `spawn_agent`, `wait_agent`, `adopt_candidate`, then final text.
///
/// The tree's own tool executors are the caller's — the harness knows nothing
/// about topology and never links it — so the caller compiles them against
/// whatever tree and ports its scenario wants and passes them in.
///
/// # Errors
///
/// Returns an error when the model, workspace, or a supplied registration is
/// invalid.
pub fn topology_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
    tree_tools: Vec<ToolRegistration>,
) -> HarnessResult<SessionRuntimeBinding> {
    let account = b"offline-loopback-topology-account";
    let model: Arc<dyn LanguageModel> = Arc::new(LoopbackModel::topology(controller, account)?);
    let mut registry = ToolRegistryBuilder::default();
    for registration in tree_tools {
        registry.register(registration)?;
    }
    let registry = registry.freeze();
    let exposures = registry
        .definitions()
        .iter()
        .map(|definition| {
            let identity = registry
                .identity(definition.name())
                .cloned()
                .ok_or("topology fixture registry identity is missing")?;
            ToolExposure::new(identity, definition.name().as_str().to_owned())
                .map_err(|_| "topology fixture exposure is invalid".into())
        })
        .collect::<HarnessResult<Vec<_>>>()?;
    let exposure = ToolExposureMap::new(registry.clone(), exposures)?;
    let definitions = exposure.definitions().to_vec();
    let configured = configured_model_with_tools(
        Arc::clone(&model),
        &descriptor(account)?,
        definitions,
        account,
        None,
        plakous::model::ToolPresentationMode::Direct,
    )?;
    let workspace = Arc::new(WorkspacePolicy::host(workspace, ".")?);
    let builder = AgentBuilder::new(configured, registry, workspace)
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .identity_sources(
            Arc::new(FixedClock::new(fixture_time()?)),
            Arc::new(SystemIdGenerator),
        );
    Ok(SessionRuntimeBinding::new(builder))
}

/// [`topology_runtime_binding`] driven by a script rather than the fixed
/// topology scenario.
///
/// The registry is the caller's tree registrations and nothing else, so a
/// scripted step's `tool` output names a topology tool by its registered name.
/// This is the only route from a spawned process to a real child: an injected
/// runtime binding replaces the whole tool registry, so unless the topology
/// tools are the registry, `spawn_agent` does not exist for the model to call.
///
/// # Errors
///
/// Returns an error when the scripted model, the tree registrations, or the
/// workspace fixture is invalid.
#[allow(
    clippy::needless_pass_by_value,
    reason = "the caller hands the script's ownership to the binding it builds"
)]
pub fn scripted_topology_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
    script: Arc<ScriptPlan>,
    cursor: Arc<AtomicU64>,
    tree_tools: Vec<ToolRegistration>,
) -> HarnessResult<SessionRuntimeBinding> {
    scripted_topology_runtime_binding_with_clock(
        workspace,
        controller,
        &script,
        cursor,
        tree_tools,
        Arc::new(FixedClock::new(fixture_time()?)),
    )
}

/// Builds a scripted topology binding with an injectable observation clock.
///
/// # Errors
/// Returns an error when the fixture model, tools or workspace are invalid.
pub fn scripted_topology_runtime_binding_with_clock(
    workspace: &Path,
    controller: BoundaryController,
    script: &Arc<ScriptPlan>,
    cursor: Arc<AtomicU64>,
    tree_tools: Vec<ToolRegistration>,
    clock: Arc<dyn plakous::model::Clock>,
) -> HarnessResult<SessionRuntimeBinding> {
    scripted_topology_runtime_binding_inner(
        workspace,
        controller,
        script,
        ScriptedOperation::Turn(cursor),
        tree_tools,
        clock,
    )
}

pub(super) fn scripted_topology_runtime_binding_inner(
    workspace: &Path,
    controller: BoundaryController,
    script: &Arc<ScriptPlan>,
    operation: ScriptedOperation,
    tree_tools: Vec<ToolRegistration>,
    clock: Arc<dyn plakous::model::Clock>,
) -> HarnessResult<SessionRuntimeBinding> {
    let account = b"offline-loopback-topology-account";
    let descriptor = script_descriptor(account, script)?;
    let model = operation.model(controller, account, descriptor.clone(), script)?;
    let mut registry = ToolRegistryBuilder::default();
    for registration in tree_tools {
        registry.register(registration)?;
    }
    let registry = registry.freeze();
    let exposures = registry
        .definitions()
        .iter()
        .map(|definition| {
            let identity = registry
                .identity(definition.name())
                .cloned()
                .ok_or("scripted topology registry identity is missing")?;
            ToolExposure::new(identity, definition.name().as_str().to_owned())
                .map_err(|_| "scripted topology exposure is invalid".into())
        })
        .collect::<HarnessResult<Vec<_>>>()?;
    let exposure = ToolExposureMap::new(registry.clone(), exposures)?;
    let definitions = exposure.definitions().to_vec();
    let configured = configured_model_with_tools(
        Arc::clone(&model),
        &descriptor,
        definitions,
        account,
        None,
        plakous::model::ToolPresentationMode::Direct,
    )?;
    let workspace = Arc::new(WorkspacePolicy::host(workspace, ".")?);
    let builder = AgentBuilder::new(configured, registry, workspace)
        .config(scripted_agent_config(script))
        .tool_policy(scripted_tool_policy(script))
        .identity_sources(clock, Arc::new(SystemIdGenerator));
    let builder = if script.scheduled_compaction {
        builder.with_scheduled_compaction_enabled(true)
    } else {
        builder
    };
    Ok(SessionRuntimeBinding::new(builder))
}

fn runtime_binding_with_input(
    workspace: &Path,
    controller: BoundaryController,
    account: &[u8],
    maximum_projected_input: u64,
    identity_namespace: Option<u128>,
) -> HarnessResult<SessionRuntimeBinding> {
    runtime_binding_inner(
        workspace,
        controller,
        account,
        maximum_projected_input,
        identity_namespace,
        false,
    )
}

fn runtime_binding_inner(
    workspace: &Path,
    controller: BoundaryController,
    account: &[u8],
    maximum_projected_input: u64,
    identity_namespace: Option<u128>,
    fail_effect: bool,
) -> HarnessResult<SessionRuntimeBinding> {
    runtime_binding_inner_with_body(
        workspace,
        controller,
        account,
        maximum_projected_input,
        identity_namespace,
        fail_effect,
        None,
    )
}

/// Builds a loopback binding whose admitted ceiling a large tool body crosses.
///
/// The placeholder rung only has something to do when a committed tool result is
/// big relative to the window, so this is the one fixture whose effect body
/// size and admitted ceiling are both dialled in.
///
/// # Errors
///
/// Returns an error when the exact model, tool, or workspace authority cannot
/// be constructed.
pub fn placeholder_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
) -> HarnessResult<SessionRuntimeBinding> {
    runtime_binding_inner_with_body(
        workspace,
        controller,
        b"offline-loopback-account",
        20_000,
        None,
        false,
        // The second read clips at the 20k ceiling. Pressure replaces the first
        // body, so the crash matrix exercises both clipping and placeholders.
        Some(5_800),
    )
}

fn runtime_binding_inner_with_body(
    workspace: &Path,
    controller: BoundaryController,
    account: &[u8],
    maximum_projected_input: u64,
    identity_namespace: Option<u128>,
    fail_effect: bool,
    body_bytes: Option<usize>,
) -> HarnessResult<SessionRuntimeBinding> {
    Ok(SessionRuntimeBinding::new(loopback_builder(
        workspace,
        controller,
        account,
        maximum_projected_input,
        identity_namespace,
        fail_effect,
        body_bytes,
    )?))
}

/// Builds a loopback binding agent construction refuses before it writes
/// anything: its instruction prefix exceeds the canonical block limit. A
/// create with it fails `Revalidation` and leaves no session behind, the way a
/// product binding whose tool projection is not admitted does.
///
/// # Errors
///
/// Returns an error when the deterministic model, tool, or workspace fixture is invalid.
pub fn construction_refused_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
) -> HarnessResult<SessionRuntimeBinding> {
    let instructions = (0..257)
        .map(|_| {
            plakous::model::InstructionBlock::new(
                plakous::model::InstructionSource::Developer,
                "over the canonical block limit",
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let builder = loopback_builder(
        workspace,
        controller,
        b"offline-loopback-account",
        10_000_000,
        None,
        false,
        None,
    )?;
    Ok(SessionRuntimeBinding::new(
        builder.instructions(instructions),
    ))
}

fn loopback_builder(
    workspace: &Path,
    controller: BoundaryController,
    account: &[u8],
    maximum_projected_input: u64,
    identity_namespace: Option<u128>,
    fail_effect: bool,
    body_bytes: Option<usize>,
) -> HarnessResult<AgentBuilder> {
    let descriptor = descriptor_with_projected_input(account, maximum_projected_input)?;
    let mut model =
        LoopbackModel::with_descriptor(controller.clone(), account, false, descriptor.clone())?;
    if let Some(identity_namespace) = identity_namespace {
        model.identity_namespace = identity_namespace;
    }
    if body_bytes.is_some() {
        model.scenario = LoopbackScenario::ContentSensitiveEffect;
    }
    let model: Arc<dyn LanguageModel> = Arc::new(model);
    let definition = ToolDefinition::new(
        ToolName::new("loopback_effect")?,
        "1.0.0",
        "Record one offline loopback effect.",
        ToolInputContract::structured(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }))?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::Mutating,
    )?;
    let configured = configured_model(
        Arc::clone(&model),
        &descriptor,
        definition.clone(),
        account,
        None,
    )?;
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("offline.loopback.effect")?,
        ToolExecutorId::new("offline.loopback.effect.v1")?,
        plakous::tools::ToolExecutorBindingIdentity::new(1, &[b"offline.loopback.effect.v1"])?,
        definition,
        Arc::new(LoopbackEffect {
            controller,
            fail: fail_effect,
            body_bytes,
        }),
    ))?;
    let workspace = Arc::new(WorkspacePolicy::host(workspace, ".")?);
    let builder = AgentBuilder::new(configured, registry.freeze(), workspace).identity_sources(
        Arc::new(FixedClock::new(fixture_time()?)),
        Arc::new(SystemIdGenerator),
    );
    let builder = builder.tool_policy(Arc::new(AlwaysAllowPolicy));
    // Only the placeholder pass fixture arms the between-rounds fence; every other
    // loopback binding keeps the default so its observations stay unchanged.
    Ok(if body_bytes.is_some() {
        builder.with_scheduled_compaction_enabled(true)
    } else {
        builder
    })
}

/// Builds the loopback binding one declarative script drives.
///
/// This is the same model, tool, descriptor and workspace authority every
/// other loopback binding uses; only the round-by-round shape comes from data
/// instead of from a Rust constructor. It exists so a spawned `plak` process
/// can select a scenario the gate names in a file.
///
/// # Errors
///
/// Returns an error when the scripted model, tool, or workspace fixture is
/// invalid.
#[allow(
    clippy::needless_pass_by_value,
    reason = "the caller hands the script's ownership to the binding it builds"
)]
pub fn scripted_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
    script: Arc<ScriptPlan>,
) -> HarnessResult<SessionRuntimeBinding> {
    scripted_runtime_binding_with_cursor(workspace, controller, script, Arc::new(AtomicU64::new(1)))
}

/// Scripted process fixture whose final response seals a managed family artifact.
///
/// # Errors
/// Returns invalid fixture construction inputs.
pub fn scripted_artifact_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
    script: &Arc<ScriptPlan>,
) -> HarnessResult<SessionRuntimeBinding> {
    scripted_runtime_binding_with_cursor_inner(
        workspace,
        controller,
        script,
        ScriptedOperation::ArtifactTurn(Arc::new(AtomicU64::new(1))),
        Vec::new(),
    )
}

/// [`scripted_runtime_binding`] over a caller-owned script cursor.
///
/// One session's script position must survive the host building more than one
/// runtime for it; a private cursor per binding restarts the script instead.
///
/// # Errors
///
/// Returns an error when the scripted model, tool, or workspace fixture is
/// invalid.
#[allow(
    clippy::needless_pass_by_value,
    reason = "the caller hands the script's ownership to the binding it builds"
)]
pub fn scripted_runtime_binding_with_cursor(
    workspace: &Path,
    controller: BoundaryController,
    script: Arc<ScriptPlan>,
    cursor: Arc<AtomicU64>,
) -> HarnessResult<SessionRuntimeBinding> {
    scripted_runtime_binding_with_extensions(workspace, controller, script, cursor, Vec::new())
}

/// [`scripted_runtime_binding_with_cursor`] whose registry also holds a host's
/// bound tool `extensions` (a template's MCP tools), each offered Direct
/// under its registration's owner.
///
/// # Errors
///
/// Returns an error when the scripted model, a tool, or the workspace fixture
/// is invalid, an extension collides with a scripted tool, or the script
/// asks for the process tools, which take no extensions.
#[allow(
    clippy::needless_pass_by_value,
    reason = "the caller hands the script's ownership to the binding it builds"
)]
pub fn scripted_runtime_binding_with_extensions(
    workspace: &Path,
    controller: BoundaryController,
    script: Arc<ScriptPlan>,
    cursor: Arc<AtomicU64>,
    extensions: Vec<ToolRegistration>,
) -> HarnessResult<SessionRuntimeBinding> {
    scripted_runtime_binding_with_cursor_inner(
        workspace,
        controller,
        &script,
        ScriptedOperation::Turn(cursor),
        extensions,
    )
}

pub(super) fn scripted_runtime_binding_with_cursor_inner(
    workspace: &Path,
    controller: BoundaryController,
    script: &Arc<ScriptPlan>,
    operation: ScriptedOperation,
    extensions: Vec<ToolRegistration>,
) -> HarnessResult<SessionRuntimeBinding> {
    let account = b"offline-loopback-account";
    let descriptor = if matches!(&operation, ScriptedOperation::ArtifactTurn(_)) {
        artifact_fixture_descriptor(account, script.maximum_projected_input)?
    } else {
        script_descriptor(account, script)?
    };
    let model = operation.model(controller.clone(), account, descriptor.clone(), script)?;
    if script.process_tools {
        if !extensions.is_empty() {
            return Err("scripted process tools take no host tool extensions".into());
        }
        return scripted_process_runtime_binding(workspace, model, &descriptor, account, script);
    }
    let definition = ToolDefinition::new(
        ToolName::new("loopback_effect")?,
        "1.0.0",
        "Record one offline loopback effect.",
        ToolInputContract::structured(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }))?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::Mutating,
    )?;
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("offline.loopback.effect")?,
        ToolExecutorId::new("offline.loopback.effect.v1")?,
        plakous::tools::ToolExecutorBindingIdentity::new(1, &[b"offline.loopback.effect.v1"])?,
        definition,
        Arc::new(LoopbackEffect {
            controller,
            fail: script.tool_fails,
            body_bytes: script.tool_result_bytes,
        }),
    ))?;
    for extension in extensions {
        registry.register(extension)?;
    }
    let registry = registry.freeze();
    let configured = configured_model_with_configured_tools(
        Arc::clone(&model),
        &descriptor,
        direct_configured_tools(&registry)?,
        account,
        None,
        plakous::model::ToolPresentationMode::Direct,
    )?;
    let workspace = Arc::new(WorkspacePolicy::host(workspace, ".")?);
    let builder = AgentBuilder::new(configured, registry, workspace)
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
    Ok(SessionRuntimeBinding::new(builder))
}

/// The scripted binding whose tools are the standard coding packs.
///
/// Same scripted model; the registry is the one
/// [`exited_child_process_runtime_binding`] builds, over a real POSIX shell, so
/// a script can start, write to, poll and terminate genuine child processes.
/// That is the only route from a spawned `plak` to the process backend's own
/// spawn and teardown.
fn scripted_process_runtime_binding(
    workspace: &Path,
    model: Arc<dyn LanguageModel>,
    descriptor: &ResolvedModelDescriptor,
    account: &[u8],
    script: &ScriptPlan,
) -> HarnessResult<SessionRuntimeBinding> {
    let process = Arc::new(ProcessSessionTools::posix_sh()?);
    let plan_state = Arc::new(InMemoryPlanState::with_identity(PlanStateIdentity::new(
        "offline.scripted.process.plan-state",
        1,
    )?));
    let mut registry = ToolRegistryBuilder::default();
    plakous::tools::register_packs(
        &plakous::tools::standard_coding_packs(plan_state, process),
        &mut registry,
    )?;
    let registry = registry.freeze();
    let exposures = registry
        .definitions()
        .iter()
        .map(|definition| {
            let identity = registry
                .identity(definition.name())
                .cloned()
                .ok_or("scripted process registry identity is missing")?;
            ToolExposure::new(identity, definition.name().as_str().to_owned())
                .map_err(|_| "scripted process exposure is invalid".into())
        })
        .collect::<HarnessResult<Vec<_>>>()?;
    let exposure = ToolExposureMap::new(registry.clone(), exposures)?;
    let definitions = exposure.definitions().to_vec();
    let configured = configured_model_with_tools(
        model,
        descriptor,
        definitions,
        account,
        None,
        plakous::model::ToolPresentationMode::Direct,
    )?;
    let workspace = Arc::new(WorkspacePolicy::host(workspace, ".")?);
    let builder = AgentBuilder::new(configured, registry, workspace)
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
    Ok(SessionRuntimeBinding::new(builder))
}

/// The policy a script asks for.
pub(super) fn scripted_tool_policy(script: &ScriptPlan) -> Arc<dyn ToolPolicy> {
    match script.tool_policy {
        ScriptToolPolicy::Allow => Arc::new(AlwaysAllowPolicy),
        ScriptToolPolicy::DecisionRequired => Arc::new(DecisionRequiredPolicy),
    }
}

pub(super) fn scripted_agent_config(script: &ScriptPlan) -> plakous::agent::AgentConfig {
    let config = plakous::agent::AgentConfig::default()
        .with_observation_budget_cap_bytes(script.observation_budget_cap_bytes);
    script.effect_decision_timeout_ms.map_or(config, |timeout| {
        config.with_effect_decision_timeout(std::time::Duration::from_millis(timeout))
    })
}

/// The scripted binding that attaches a real Code Mode profile.
///
/// A `composition` label on the session record changes nothing on its own: the
/// binding still exposes the bare tools and the turn takes the ordinary direct
/// path. This is what actually puts the model behind the `exec` and `wait`
/// markers over the Codex minimal nested catalog, so a scripted cell runs a
/// JavaScript program in the sidecar and its nested calls are real.
///
/// Built the way the `tests/fixtures/facade-consumer` inventory builds it, over
/// the public surface only.
///
/// # Errors
///
/// Returns an error when the registry, projection target, profile, sidecar
/// artifact or workspace fixture is invalid.
#[allow(
    clippy::needless_pass_by_value,
    reason = "the caller hands the script's ownership to the binding it builds"
)]
pub fn scripted_code_mode_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
    script: Arc<ScriptPlan>,
    cursor: Arc<AtomicU64>,
    sidecar: &Path,
    telemetry: Option<Arc<plakous::agent::TelemetryEmitter>>,
) -> HarnessResult<SessionRuntimeBinding> {
    scripted_code_mode_runtime_binding_with_extensions(
        workspace,
        controller,
        script,
        cursor,
        sidecar,
        telemetry,
        Vec::new(),
    )
}
