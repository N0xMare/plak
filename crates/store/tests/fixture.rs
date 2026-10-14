//! Shared reconstruction-complete checkpoint fixtures for store tests.

use std::error::Error;
use std::str::FromStr;

use plakous_agent::internal::{
    BindingReference, CheckpointObjectBundle, CompilerModelFacts, CompilerRuntimeFacts,
    CompilerToolFacts, FeatureSupportFact, ForkLineage, HARNESS_PLAN_COMPONENT_FORMAT_VERSION,
    HarnessCompilerInputs, HarnessPlanComponentEnvelope, HarnessProfileCompiler, HarnessProfileId,
    HarnessProfileSpecBuilder, ModelRouteBinding, ProducerProvenance, ReconstructiveComponent,
    RevalidationFact, RuntimeCanonicalState, RuntimeCheckpoint, RuntimeCheckpointCodec,
    RuntimeComponentKind, RuntimeContextState, RuntimeDurability, RuntimeLifecycle,
    RuntimeManifest, RuntimeOperationLedger, RuntimeTopology, ToolRuntimeManifest,
};
use plakous_model::{
    Actor, BranchId, CachePolicy, CanonicalHistory, CapabilitySet, CheckpointId,
    CompiledProviderConfiguration, ContentBlock, ContextAdmissionRequirement, ConversationItem,
    CredentialSelectionId, Digest, DigestDomain, EffectiveProviderConfiguration, EntryId,
    FamilyPolicyEnvelope, GenerationLimits, HistorySegment, HistorySegmentKind,
    LocalDurabilityDisposition, ModelAdapterProfileId, ModelFamilyId, ModelId,
    ModelImplementationIdentity, NativeOptions, OutputBoundDisposition, OutputPolicy,
    ParallelToolDisposition, ProtocolId, Provenance, ProviderAuthorityClassId,
    ProviderBehaviorConfiguration, ProviderProfileRevisionId, ProviderRouteSelection,
    ProviderSemanticEvidence, ProviderStorageDisposition, ReasoningEffortDisposition,
    ReasoningIncludeDisposition, ReasoningPolicy, ReasoningSummaryDisposition, ReplayDisposition,
    ResolvedModelDescriptor, ResolvedProviderAuthorityBinding, ResponseStateDisposition,
    RetentionDisposition, RouteId, RuntimeModelAuthority, SelectedProviderConfiguration, SessionId,
    StableRequestPrefix, TextBlock, Timestamp, ToolChoice, WorkingProjection,
};

pub(crate) type TestResult<T = ()> = Result<T, Box<dyn Error>>;

pub(crate) fn id<T>(value: u128) -> TestResult<T>
where
    T: FromStr,
    T::Err: Error + 'static,
{
    let hex = format!("{value:032x}");
    let text = format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    );
    Ok(text.parse()?)
}

fn digest(domain: DigestDomain, bytes: &[u8]) -> TestResult<Digest> {
    Ok(Digest::sha256(domain, 1, &[bytes])?)
}

fn component(kind: RuntimeComponentKind, bytes: &[u8]) -> TestResult<ReconstructiveComponent> {
    let revision = match kind {
        // The V2 tool descriptor codec revision.
        RuntimeComponentKind::ToolRegistry | RuntimeComponentKind::ToolExposure => 3,
        _ => 1,
    };
    Ok(ReconstructiveComponent::new(
        kind,
        revision,
        bytes.to_vec(),
    )?)
}

fn provider_configuration(
    durability: RuntimeDurability,
) -> TestResult<CompiledProviderConfiguration> {
    let descriptor = ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("test.protocol")?,
        RouteId::new("test.route")?,
        ModelId::new("test-model")?,
        Some(ModelId::new("test-model")?),
        Some(ModelFamilyId::new("test-family")?),
        ModelAdapterProfileId::new("test.adapter")?,
        CapabilitySet::default(),
    );
    let behavior = ProviderBehaviorConfiguration {
        processing_tier: plakous_model::ProcessingTierDisposition::Standard,
        tool_mode: plakous_model::ToolPresentationMode::Direct,
        reasoning_effort: ReasoningEffortDisposition::Disabled,
        reasoning_summary: ReasoningSummaryDisposition::Omit,
        reasoning_include: ReasoningIncludeDisposition::Omit,
        replay: ReplayDisposition::Disabled,
        cache: CachePolicy::Disabled,
        tools: Vec::new(),
        hosted_tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        parallel_tools: ParallelToolDisposition::Disabled,
        output: OutputPolicy::default(),
        output_bound: OutputBoundDisposition::NoRequestBound,
        storage: ProviderStorageDisposition::OmittedRouteContract,
        retention: RetentionDisposition::RouteContract {
            profile: descriptor.profile().clone(),
            revision: ProviderProfileRevisionId::new("store-retention-v1")?,
        },
        response_state: ResponseStateDisposition::Stateless,
        context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
        local_durability: match durability {
            RuntimeDurability::Required => LocalDurabilityDisposition::DurableRequired,
            RuntimeDurability::BestEffort => LocalDurabilityDisposition::BestEffort,
            RuntimeDurability::Disabled => LocalDurabilityDisposition::Ephemeral,
        },
    };
    let selected = SelectedProviderConfiguration::new(
        ProviderRouteSelection::new(
            descriptor.protocol().clone(),
            descriptor.route().clone(),
            descriptor.profile().clone(),
            ProviderProfileRevisionId::new("store-integration-v1")?,
            ModelAdapterProfileId::new("store.auth")?,
            ProviderProfileRevisionId::new("store-auth-v1")?,
            ProviderAuthorityClassId::new("store-test")?,
            CredentialSelectionId::new("store-test-credential")?,
            descriptor.requested_model().clone(),
        ),
        behavior.clone(),
    )?;
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        digest(DigestDomain::ModelBinding, b"store-account")?,
        digest(DigestDomain::ModelBinding, b"store-credential-authority")?,
        1,
        Timestamp::new(10, 0)?,
        Some(Timestamp::new(10_000, 0)?),
    )?;
    let stable_authority = ResolvedProviderAuthorityBinding::new(
        &descriptor,
        &authority,
        ProviderSemanticEvidence::new(1, b"store-provider-semantics-v1".to_vec())?,
    )?;
    let family_policy = FamilyPolicyEnvelope::new(
        descriptor.model_family().cloned().ok_or("model family")?,
        1,
        1,
        1,
        b"store-family-policy-v1".to_vec(),
    )?;
    let effective = EffectiveProviderConfiguration::compile(
        &selected,
        descriptor.clone(),
        ModelImplementationIdentity::release_owned(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            &descriptor,
            1,
        )?,
        stable_authority,
        behavior,
        family_policy,
    )?;
    Ok(CompiledProviderConfiguration::new(selected, effective)?)
}

fn plan_component(
    configuration: &CompiledProviderConfiguration,
) -> TestResult<(ReconstructiveComponent, Digest)> {
    let spec = HarnessProfileSpecBuilder::new()
        .id(HarnessProfileId::new("store.conformance")?)
        .base_instructions("exact semantic store conformance")
        .build()?;
    let model = CompilerModelFacts::new(
        configuration.selected_identity(),
        configuration.effective_identity(),
        ProtocolId::new("test.protocol")?,
        RouteId::new("test.route")?,
        ModelId::new("test-model")?,
        ModelAdapterProfileId::new("test.adapter")?,
        digest(DigestDomain::ModelBinding, b"model")?,
        FeatureSupportFact::Unsupported,
        FeatureSupportFact::Unsupported,
        FeatureSupportFact::Unsupported,
    );
    let tools = CompilerToolFacts::new(
        digest(DigestDomain::ToolDefinition, b"tools")?,
        vec!["read_file".to_owned()],
    )?;
    let runtime = CompilerRuntimeFacts::new(
        digest(DigestDomain::ExecutionEnvironment, b"environment")?,
        plakous_agent::internal::MAX_INSTRUCTION_BYTES,
        false,
        false,
        false,
    );
    let inputs = HarnessCompilerInputs::new(model, tools, runtime, Vec::new())?;
    let plan = HarnessProfileCompiler::new().compile(&spec, &inputs)?;
    let identity = *plan.plan_digest();
    let bytes = HarnessPlanComponentEnvelope::capture(
        &spec,
        &inputs,
        &plan,
        plakous_agent::internal::RUNTIME_POLICY_REVISION,
    )?
    .encode()?;
    Ok((
        ReconstructiveComponent::new(
            RuntimeComponentKind::HarnessPlan,
            HARNESS_PLAN_COMPONENT_FORMAT_VERSION,
            bytes,
        )?,
        identity,
    ))
}

pub(crate) fn checkpoint(
    topology: RuntimeTopology,
    history_len: u32,
    durability: RuntimeDurability,
) -> TestResult<RuntimeCheckpoint> {
    let context =
        RuntimeContextState::genesis(topology.session_id, topology.branch_id, [0x51; 32])?;
    checkpoint_with_context(topology, history_len, durability, context)
}

pub(crate) fn checkpoint_with_context(
    topology: RuntimeTopology,
    history_len: u32,
    durability: RuntimeDurability,
    context: RuntimeContextState,
) -> TestResult<RuntimeCheckpoint> {
    checkpoint_with_payload(topology, history_len, durability, context, "")
}

pub(crate) fn checkpoint_with_payload(
    topology: RuntimeTopology,
    history_len: u32,
    durability: RuntimeDurability,
    context: RuntimeContextState,
    payload: &str,
) -> TestResult<RuntimeCheckpoint> {
    let provider_configuration = provider_configuration(durability)?;
    let (harness_plan, plan_identity) = plan_component(&provider_configuration)?;
    let prefix = StableRequestPrefix::new(
        provider_configuration.selected_identity(),
        provider_configuration.effective_identity(),
        provider_configuration
            .effective()
            .behavior()
            .processing_tier
            .clone(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        OutputPolicy::default(),
        ReasoningPolicy::Disabled,
        ToolChoice::default(),
        CachePolicy::Disabled,
        GenerationLimits::default(),
        NativeOptions::default(),
        Some(plan_identity),
    )?;
    let mut segments = Vec::new();
    for index in 0..history_len {
        let item = ConversationItem::new(
            id::<EntryId>(10_000 + u128::from(index))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new(
                format!("turn {index}{payload}"),
                None,
            )?)],
            Provenance::UserInput,
        )?;
        segments.push(HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![item],
        )?);
    }
    let canonical = RuntimeCanonicalState::new(
        prefix,
        CanonicalHistory::new(segments)?,
        WorkingProjection::default(),
        context,
    )?;
    let account = BindingReference::new("account-authority", "account-ref", 1)?;
    let profile = BindingReference::new("profile-authority", "profile-ref", 1)?;
    let admission = RevalidationFact::new(
        account.clone(),
        Timestamp::new(10, 0)?,
        Some(Timestamp::new(10_000, 0)?),
        component(RuntimeComponentKind::AdmissionEvidence, b"admission-v1")?,
    )?;
    let binding = ModelRouteBinding::new(
        ProtocolId::new("test.protocol")?,
        RouteId::new("test.route")?,
        ModelId::new("test-model")?,
        Some(ModelId::new("test-model")?),
        Some(ModelFamilyId::new("test-family")?),
        ModelAdapterProfileId::new("test.adapter")?,
        account,
        profile,
        provider_configuration
            .effective()
            .descriptor()
            .semantic_binding_digest()?,
        component(RuntimeComponentKind::ModelDescriptor, b"descriptor-v1")?,
        admission,
    )?;
    let tools = ToolRuntimeManifest::new(
        component(RuntimeComponentKind::ToolRegistry, b"tool-registry-v1")?,
        component(RuntimeComponentKind::ToolExposure, b"tool-exposure-v1")?,
        component(
            RuntimeComponentKind::ExecutionEnvironment,
            b"environment-v1",
        )?,
        component(RuntimeComponentKind::EffectPolicy, b"effect-policy-v1")?,
        BindingReference::new("tool-policy-authority", "policy-ref", 1)?,
    )?;
    let manifest = RuntimeManifest::new(
        ProducerProvenance::new("plakous", "store-conformance", "offline")?,
        topology,
        RuntimeLifecycle::Quiescent,
        harness_plan,
        provider_configuration,
        canonical,
        binding,
        tools,
        RuntimeOperationLedger::new(Vec::new(), Vec::new())?,
        None,
        None,
        plakous_agent::internal::JobLedger::default(),
        durability,
    )?;
    Ok(RuntimeCheckpoint::new(manifest)?)
}

pub(crate) fn topology(
    session_id: SessionId,
    branch_id: BranchId,
    checkpoint_id: CheckpointId,
    parent_checkpoint_id: Option<CheckpointId>,
    branch_revision: u64,
    fork: Option<ForkLineage>,
    lease_fence: u64,
) -> TestResult<RuntimeTopology> {
    Ok(RuntimeTopology {
        session_id,
        branch_id,
        checkpoint_id,
        parent_checkpoint_id,
        branch_revision,
        fork,
        lease_fence,
        commit_identity: Digest::sha256(
            DigestDomain::ProviderOperation,
            1,
            &[b"store-commit-v1", checkpoint_id.as_uuid().as_bytes()],
        )?,
    })
}

pub(crate) fn bundle(
    checkpoint: &RuntimeCheckpoint,
    prior: Option<&CheckpointObjectBundle>,
) -> TestResult<CheckpointObjectBundle> {
    Ok(RuntimeCheckpointCodec::bundle(checkpoint, prior)?)
}

// Missing-root/base guards live after the sealed bundle's port validation.
// Both backends use this same valid pair before damaging a private candidate.
pub(crate) fn integrity_publication_pair()
-> TestResult<(CheckpointObjectBundle, CheckpointObjectBundle)> {
    let session = id(1)?;
    let branch = id(2)?;
    let base_id = id(3)?;
    let base = checkpoint(
        topology(session, branch, base_id, None, 0, None, 1)?,
        1,
        RuntimeDurability::Required,
    )?;
    let base = bundle(&base, None)?;
    let next = checkpoint(
        topology(session, branch, id(4)?, Some(base_id), 1, None, 1)?,
        2,
        RuntimeDurability::Required,
    )?;
    Ok((base.clone(), bundle(&next, Some(&base))?))
}
