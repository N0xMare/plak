//! Backend-parameterized semantic `SessionStore` conformance.

#[cfg(test)]
use std::collections::BTreeSet;
use std::error::Error;
use std::num::{NonZeroU64, NonZeroUsize};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

#[cfg(test)]
use plakous_agent::internal::SessionStoreCapacity;
use plakous_agent::internal::{
    AcquireBranch, ArchiveSession, BindingReference, BranchLease, BranchTransitionMode,
    CheckpointObjectBundle, CommitCheckpoint, CompilerModelFacts, CompilerRuntimeFacts,
    CompilerToolFacts, CreateSession, DeleteSession, FeatureSupportFact, ForkBranch, ForkLineage,
    HARNESS_PLAN_COMPONENT_FORMAT_VERSION, HarnessCompilerInputs, HarnessPlanComponentEnvelope,
    HarnessProfileCompiler, HarnessProfileId, HarnessProfileSpecBuilder, LeaseClock, LeaseDomain,
    LeaseInstant, ModelRouteBinding, ProducerProvenance, PruneBranch, RUNTIME_FORMAT_EPOCH,
    ReconstructiveComponent, RecoverFork, RevalidationFact, RuntimeCanonicalState,
    RuntimeCheckpoint, RuntimeCheckpointCodec, RuntimeComponentKind, RuntimeContextState,
    RuntimeDurability, RuntimeLifecycle, RuntimeManifest, RuntimeOperationLedger, RuntimeTopology,
    SessionQuery, SessionRef, SessionRevision, SessionStore, SessionStoreError, SessionWorkflowId,
    StoreAdmission, StoreCapacityBound, StoreInvalidReason, ToolRuntimeManifest,
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
    RetentionDisposition, RouteId, RunId, RuntimeModelAuthority, SelectedProviderConfiguration,
    SessionId, StableRequestPrefix, TextBlock, Timestamp, ToolChoice, WorkingProjection,
};
#[cfg(test)]
use plakous_store::{MemorySessionStore, MemoryStoreLimits};

pub(crate) type TestResult<T = ()> = Result<T, Box<dyn Error>>;

#[derive(Debug)]
pub(crate) struct ManualLeaseClock(Mutex<Timestamp>);

impl ManualLeaseClock {
    pub(crate) fn new(timestamp: Timestamp) -> Self {
        Self(Mutex::new(timestamp))
    }

    fn set(&self, timestamp: Timestamp) {
        match self.0.lock() {
            Ok(mut current) => *current = timestamp,
            Err(poisoned) => *poisoned.into_inner() = timestamp,
        }
    }
}

impl LeaseClock for ManualLeaseClock {
    fn now(&self) -> Result<LeaseInstant, SessionStoreError> {
        let value = match self.0.lock() {
            Ok(current) => *current,
            Err(poisoned) => *poisoned.into_inner(),
        };
        LeaseInstant::new(
            LeaseDomain::new([1; 16], [2; 32]),
            value.seconds(),
            value.nanoseconds(),
        )
    }
}

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

pub(crate) fn workflow(value: u8) -> TestResult<SessionWorkflowId> {
    Ok(SessionWorkflowId::new([value; 32])?)
}

fn digest(domain: DigestDomain, bytes: &[u8]) -> TestResult<Digest> {
    Ok(Digest::sha256(domain, 1, &[bytes])?)
}

pub(crate) fn semantic_request(bytes: &[u8]) -> TestResult<Digest> {
    digest(DigestDomain::RequestPlan, bytes)
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
    rebound: bool,
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
    let account = if rebound {
        b"store-account-rebound".as_slice()
    } else {
        b"store-account".as_slice()
    };
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        digest(DigestDomain::ModelBinding, account)?,
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
    checkpoint_with_context(topology, history_len, durability, context, false)
}

pub(crate) fn checkpoint_with_context(
    topology: RuntimeTopology,
    history_len: u32,
    durability: RuntimeDurability,
    context: RuntimeContextState,
    rebound: bool,
) -> TestResult<RuntimeCheckpoint> {
    checkpoint_with_payload(topology, history_len, durability, context, rebound, "")
}

/// A checkpoint whose every history item carries `payload`, so checkpoints
/// with distinct payloads share no history objects.
pub(crate) fn checkpoint_with_payload(
    topology: RuntimeTopology,
    history_len: u32,
    durability: RuntimeDurability,
    context: RuntimeContextState,
    rebound: bool,
    payload: &str,
) -> TestResult<RuntimeCheckpoint> {
    let provider_configuration = provider_configuration(rebound, durability)?;
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
    let account_reference = if rebound {
        "account-ref-rebound"
    } else {
        "account-ref"
    };
    let account = BindingReference::new("account-authority", account_reference, 1)?;
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

#[allow(
    clippy::too_many_lines,
    reason = "one ordered corpus is reused unchanged by every backend factory"
)]
pub(crate) async fn run_semantic_conformance<F>(
    durability: RuntimeDurability,
    expected_effective_durability: RuntimeDurability,
    open: F,
) -> TestResult
where
    F: FnOnce(Arc<dyn LeaseClock>) -> TestResult<Arc<dyn SessionStore>>,
{
    let clock = Arc::new(ManualLeaseClock::new(Timestamp::new(100, 0)?));
    let store = open(clock.clone())?;
    assert_eq!(
        store.lease_now()?,
        LeaseInstant::new(LeaseDomain::new([1; 16], [2; 32]), 100, 0)?
    );
    let session_id = id::<SessionId>(1)?;
    let source_branch = id::<BranchId>(2)?;
    let target_branch = id::<BranchId>(3)?;
    let rebind_branch = id::<BranchId>(4)?;
    let genesis_id = id::<CheckpointId>(10)?;
    let commit_id = id::<CheckpointId>(11)?;
    let stale_id = id::<CheckpointId>(12)?;
    let fork_id = id::<CheckpointId>(13)?;
    let rebind_id = id::<CheckpointId>(14)?;
    let target_commit_id = id::<CheckpointId>(15)?;
    let owner = id::<RunId>(20)?;
    let successor_owner = id::<RunId>(21)?;
    let invalid_workflow = workflow(1)?;
    let wrong_source_workflow = workflow(2)?;
    let changed_workflow = workflow(3)?;
    let unchanged_rebind_workflow = workflow(4)?;
    let fork_workflow = workflow(5)?;
    let reused_target_workflow = workflow(6)?;
    let rebind_workflow = workflow(7)?;
    let foreign_domain_workflow = workflow(8)?;
    let fork_semantic_request = semantic_request(b"ordinary fork")?;
    let different_semantic_request = semantic_request(b"different fork")?;
    let rebind_semantic_request = semantic_request(b"rebind")?;
    let lease_seconds = NonZeroU64::new(10).ok_or("nonzero lease")?;
    let source_ref = SessionRef::new(session_id, source_branch);

    let genesis_checkpoint = checkpoint(
        topology(session_id, source_branch, genesis_id, None, 0, None, 1)?,
        1,
        durability,
    )?;
    let genesis_bundle = bundle(&genesis_checkpoint, None)?;
    assert_eq!(RUNTIME_FORMAT_EPOCH, 25);
    assert!(
        genesis_bundle
            .supplied_objects()
            .iter()
            .all(|object| object.address().codec_revision() == RUNTIME_FORMAT_EPOCH)
    );
    let create = CreateSession::new(
        source_ref,
        Timestamp::new(50, 0)?,
        owner,
        lease_seconds,
        genesis_bundle.clone(),
    );
    let created = store.create(create.clone()).await?;
    assert_eq!(created, store.create(create.clone()).await?);
    assert_eq!(created, store.recover_create(create.clone()).await?);
    let conflicting_create = CreateSession::new(
        source_ref,
        Timestamp::new(51, 0)?,
        owner,
        lease_seconds,
        genesis_bundle.clone(),
    );
    assert_eq!(
        store.create(conflicting_create).await,
        Err(SessionStoreError::IdentityConflict)
    );
    assert_eq!(
        created.branch(),
        store.load_branch(source_ref).await?.head()
    );
    let loaded_genesis = store.load_checkpoint(genesis_id).await?;
    assert_eq!(loaded_genesis.checkpoint().as_ref(), &genesis_checkpoint);
    assert_eq!(loaded_genesis.bundle().base_root(), None);

    let committed_checkpoint = checkpoint(
        topology(
            session_id,
            source_branch,
            commit_id,
            Some(genesis_id),
            1,
            None,
            1,
        )?,
        2,
        durability,
    )?;
    let committed_bundle = bundle(&committed_checkpoint, Some(loaded_genesis.bundle()))?;
    assert_eq!(
        committed_bundle.base_root(),
        Some(loaded_genesis.bundle().root())
    );
    assert!(committed_bundle.stats().new_object_count() < committed_bundle.stats().object_count());
    let commit = CommitCheckpoint::new(
        created.lease().clone(),
        created.branch(),
        committed_bundle.clone(),
    );
    let commit_receipt = store.commit(commit.clone()).await?;
    assert_eq!(commit_receipt, store.commit(commit.clone()).await?);
    assert_eq!(
        store.recover_commit(commit_id).await?.receipt(),
        commit_receipt
    );
    assert_eq!(
        store
            .commit(CommitCheckpoint::new(
                created.lease().clone(),
                commit_receipt.head(),
                committed_bundle.clone(),
            ))
            .await,
        Err(SessionStoreError::IdentityConflict)
    );
    assert_eq!(
        store.load_branch(source_ref).await?.head(),
        commit_receipt.head()
    );
    assert_eq!(
        store
            .load_checkpoint(genesis_id)
            .await?
            .checkpoint()
            .as_ref(),
        &genesis_checkpoint
    );

    let stale_checkpoint = checkpoint(
        topology(
            session_id,
            source_branch,
            stale_id,
            Some(genesis_id),
            1,
            None,
            1,
        )?,
        2,
        durability,
    )?;
    let stale = CommitCheckpoint::new(
        created.lease().clone(),
        created.branch(),
        bundle(&stale_checkpoint, Some(&genesis_bundle))?,
    );
    assert_eq!(store.commit(stale).await, Err(SessionStoreError::Conflict));

    let fork_lineage = ForkLineage {
        session_id,
        branch_id: source_branch,
        checkpoint_id: commit_id,
        branch_revision: 1,
    };
    let invalid_target_checkpoint = checkpoint(
        topology(
            session_id,
            target_branch,
            fork_id,
            Some(commit_id),
            0,
            Some(fork_lineage),
            1,
        )?,
        2,
        durability,
    )?;
    let invalid_target_bundle = bundle(&invalid_target_checkpoint, Some(&committed_bundle))?;
    let invalid_fork = ForkBranch::new(
        invalid_workflow,
        fork_semantic_request,
        created.lease().clone(),
        SessionRevision::INITIAL,
        commit_receipt.head(),
        target_branch,
        owner,
        lease_seconds,
        invalid_target_bundle,
    );
    assert_eq!(
        store.fork(invalid_fork.clone()).await,
        Err(SessionStoreError::Invalid(
            StoreInvalidReason::InvalidBranchContextTransition
        ))
    );
    assert_eq!(
        store
            .recover_fork(RecoverFork::new(
                invalid_workflow,
                fork_semantic_request,
                created.lease().clone(),
                commit_receipt.head(),
                target_branch,
            ))
            .await,
        Err(SessionStoreError::NotFound)
    );

    let source_context = committed_checkpoint.manifest().canonical().context();
    let target_topology = topology(
        session_id,
        target_branch,
        fork_id,
        Some(commit_id),
        0,
        Some(fork_lineage),
        1,
    )?;
    let wrong_source_context =
        source_context.ordinary_fork(stale_id, session_id, target_branch, [0x61; 32])?;
    let wrong_source_checkpoint =
        checkpoint_with_context(target_topology, 2, durability, wrong_source_context, false)?;
    assert_eq!(
        store
            .fork(ForkBranch::new(
                wrong_source_workflow,
                fork_semantic_request,
                created.lease().clone(),
                SessionRevision::INITIAL,
                commit_receipt.head(),
                target_branch,
                owner,
                lease_seconds,
                bundle(&wrong_source_checkpoint, Some(&committed_bundle))?,
            ))
            .await,
        Err(SessionStoreError::Invalid(
            StoreInvalidReason::InvalidBranchContextTransition
        ))
    );

    let changed_ordinary_context =
        source_context.ordinary_fork(commit_id, session_id, target_branch, [0x62; 32])?;
    let changed_ordinary_checkpoint = checkpoint_with_context(
        target_topology,
        2,
        durability,
        changed_ordinary_context,
        true,
    )?;
    assert_eq!(
        store
            .fork(ForkBranch::new(
                changed_workflow,
                fork_semantic_request,
                created.lease().clone(),
                SessionRevision::INITIAL,
                commit_receipt.head(),
                target_branch,
                owner,
                lease_seconds,
                bundle(&changed_ordinary_checkpoint, Some(&committed_bundle))?,
            ))
            .await,
        Err(SessionStoreError::Invalid(
            StoreInvalidReason::OrdinaryForkChangedSemantics
        ))
    );

    let unchanged_rebind_context =
        source_context.rebind(commit_id, session_id, target_branch, [0x63; 32])?;
    let unchanged_rebind_checkpoint = checkpoint_with_context(
        target_topology,
        2,
        durability,
        unchanged_rebind_context,
        false,
    )?;
    assert_eq!(
        store
            .fork(
                ForkBranch::new(
                    unchanged_rebind_workflow,
                    rebind_semantic_request,
                    created.lease().clone(),
                    SessionRevision::INITIAL,
                    commit_receipt.head(),
                    target_branch,
                    owner,
                    lease_seconds,
                    bundle(&unchanged_rebind_checkpoint, Some(&committed_bundle))?,
                )
                .with_transition(BranchTransitionMode::Rebind),
            )
            .await,
        Err(SessionStoreError::Invalid(
            StoreInvalidReason::RebindConfigurationUnchanged
        ))
    );

    let target_context =
        source_context.ordinary_fork(commit_id, session_id, target_branch, [0x64; 32])?;
    let target_checkpoint =
        checkpoint_with_context(target_topology, 2, durability, target_context, false)?;
    let target_bundle = bundle(&target_checkpoint, Some(&committed_bundle))?;
    let fork = ForkBranch::new(
        fork_workflow,
        fork_semantic_request,
        created.lease().clone(),
        SessionRevision::INITIAL,
        commit_receipt.head(),
        target_branch,
        owner,
        lease_seconds,
        target_bundle.clone(),
    );
    let stale_fork = ForkBranch::new(
        fork_workflow,
        fork_semantic_request,
        created.lease().clone(),
        SessionRevision::INITIAL,
        created.branch(),
        target_branch,
        owner,
        lease_seconds,
        target_bundle.clone(),
    );
    assert_eq!(
        store.fork(stale_fork).await,
        Err(SessionStoreError::Conflict)
    );
    let reused_target = ForkBranch::new(
        reused_target_workflow,
        fork_semantic_request,
        created.lease().clone(),
        SessionRevision::INITIAL,
        commit_receipt.head(),
        source_branch,
        owner,
        lease_seconds,
        target_bundle.clone(),
    );
    assert_eq!(
        store.fork(reused_target).await,
        Err(SessionStoreError::IdentityConflict)
    );
    assert_eq!(
        store
            .fork(ForkBranch::new(
                foreign_domain_workflow,
                digest(DigestDomain::Blob, b"not a semantic request")?,
                created.lease().clone(),
                SessionRevision::INITIAL,
                commit_receipt.head(),
                target_branch,
                owner,
                lease_seconds,
                target_bundle.clone(),
            ))
            .await,
        Err(SessionStoreError::Invalid(
            StoreInvalidReason::ForkRequestDigestDomain
        ))
    );
    let fork_receipt = store.fork(fork.clone()).await?;
    assert_eq!(fork_receipt, store.fork(fork.clone()).await?);
    assert_eq!(
        fork_receipt.operation().semantic_request(),
        fork_semantic_request
    );
    assert_eq!(
        fork_receipt.operation().expected_session_revision(),
        SessionRevision::INITIAL,
        "the receipt retains the historical publication CAS revision"
    );
    assert_eq!(
        fork_receipt,
        store
            .recover_fork(RecoverFork::new(
                fork_workflow,
                fork_semantic_request,
                created.lease().clone(),
                commit_receipt.head(),
                target_branch,
            ))
            .await?
    );
    assert_eq!(
        store
            .recover_fork(RecoverFork::new(
                fork_workflow,
                different_semantic_request,
                created.lease().clone(),
                commit_receipt.head(),
                target_branch,
            ))
            .await,
        Err(SessionStoreError::IdentityConflict)
    );
    assert_eq!(
        store
            .recover_fork(RecoverFork::new(
                fork_workflow,
                fork_semantic_request,
                created.lease().clone(),
                created.branch(),
                target_branch,
            ))
            .await,
        Err(SessionStoreError::IdentityConflict)
    );
    assert_eq!(
        store
            .recover_fork(RecoverFork::new(
                fork_workflow,
                fork_semantic_request,
                created.lease().clone(),
                commit_receipt.head(),
                rebind_branch,
            ))
            .await,
        Err(SessionStoreError::IdentityConflict)
    );
    assert_eq!(fork_receipt.source(), commit_receipt.head());
    assert_eq!(
        store
            .load_branch(SessionRef::new(session_id, target_branch))
            .await?
            .head(),
        fork_receipt.target()
    );
    assert_eq!(
        store
            .fork(ForkBranch::new(
                fork_workflow,
                fork_semantic_request,
                created.lease().clone(),
                SessionRevision::INITIAL,
                commit_receipt.head(),
                target_branch,
                successor_owner,
                lease_seconds,
                target_bundle.clone(),
            ))
            .await?,
        fork_receipt
    );
    assert_eq!(
        store
            .fork(ForkBranch::new(
                fork_workflow,
                different_semantic_request,
                created.lease().clone(),
                SessionRevision::INITIAL,
                commit_receipt.head(),
                target_branch,
                successor_owner,
                lease_seconds,
                target_bundle.clone(),
            ))
            .await,
        Err(SessionStoreError::IdentityConflict)
    );
    assert_eq!(
        store
            .fork(ForkBranch::new(
                reused_target_workflow,
                fork_semantic_request,
                created.lease().clone(),
                SessionRevision::INITIAL,
                commit_receipt.head(),
                target_branch,
                successor_owner,
                lease_seconds,
                target_bundle.clone(),
            ))
            .await,
        Err(SessionStoreError::Conflict)
    );
    assert_eq!(
        store
            .fork(ForkBranch::new(
                fork_workflow,
                fork_semantic_request,
                created.lease().clone(),
                SessionRevision::INITIAL,
                commit_receipt.head(),
                rebind_branch,
                owner,
                lease_seconds,
                target_bundle.clone(),
            ))
            .await,
        Err(SessionStoreError::IdentityConflict)
    );
    assert_eq!(
        store
            .recover_fork(
                RecoverFork::new(
                    fork_workflow,
                    fork_semantic_request,
                    created.lease().clone(),
                    commit_receipt.head(),
                    target_branch,
                )
                .with_transition(BranchTransitionMode::Rebind),
            )
            .await,
        Err(SessionStoreError::IdentityConflict)
    );
    store.release(fork_receipt.lease().clone()).await?;

    let rebind_lineage = ForkLineage {
        session_id,
        branch_id: source_branch,
        checkpoint_id: commit_id,
        branch_revision: 1,
    };
    let rebind_topology = topology(
        session_id,
        rebind_branch,
        rebind_id,
        Some(commit_id),
        0,
        Some(rebind_lineage),
        1,
    )?;
    let rebind_context = source_context.rebind(commit_id, session_id, rebind_branch, [0x65; 32])?;
    let rebind_checkpoint =
        checkpoint_with_context(rebind_topology, 2, durability, rebind_context, true)?;
    let rebind = ForkBranch::new(
        rebind_workflow,
        rebind_semantic_request,
        created.lease().clone(),
        fork_receipt.session().revision(),
        commit_receipt.head(),
        rebind_branch,
        owner,
        lease_seconds,
        bundle(&rebind_checkpoint, Some(&committed_bundle))?,
    )
    .with_transition(BranchTransitionMode::Rebind);
    let rebind_receipt = store.fork(rebind.clone()).await?;
    assert_eq!(rebind_receipt, store.fork(rebind.clone()).await?);
    assert_eq!(
        rebind_receipt,
        store
            .recover_fork(
                RecoverFork::new(
                    rebind_workflow,
                    rebind_semantic_request,
                    created.lease().clone(),
                    commit_receipt.head(),
                    rebind_branch,
                )
                .with_transition(BranchTransitionMode::Rebind),
            )
            .await?
    );
    store.release(rebind_receipt.lease().clone()).await?;
    assert_eq!(
        store.load_branch(source_ref).await?.head(),
        commit_receipt.head()
    );
    assert_eq!(
        store
            .load_branch(SessionRef::new(session_id, target_branch))
            .await?
            .head(),
        fork_receipt.target()
    );

    store.release(created.lease().clone()).await?;
    let acquisition = AcquireBranch::new(source_ref, successor_owner, lease_seconds);
    let acquired = store.acquire(acquisition).await?;
    assert_eq!(acquired, store.acquire(acquisition).await?);
    assert!(acquired.fence() > created.lease().fence());
    let source_before_recovery = store.load_branch(source_ref).await?;
    let fork_recovery = RecoverFork::new(
        fork_workflow,
        fork_semantic_request,
        acquired.clone(),
        commit_receipt.head(),
        target_branch,
    );
    let recovered_fork = store.recover_fork(fork_recovery.clone()).await?;
    assert_eq!(recovered_fork, fork_receipt);
    assert_eq!(store.load_branch(source_ref).await?, source_before_recovery);
    assert_eq!(
        source_before_recovery.session().revision(),
        rebind_receipt.session().revision(),
        "receipt recovery does not require the current topology revision to remain original"
    );
    store.release(recovered_fork.lease().clone()).await?;
    let target_acquisition = AcquireBranch::new(
        SessionRef::new(session_id, target_branch),
        successor_owner,
        lease_seconds,
    );
    let target_lease = store.acquire(target_acquisition).await?;
    assert!(target_lease.fence() > recovered_fork.lease().fence());
    let advanced_target_checkpoint = checkpoint_with_context(
        topology(
            session_id,
            target_branch,
            target_commit_id,
            Some(fork_id),
            1,
            Some(fork_lineage),
            target_lease.fence().get(),
        )?,
        3,
        durability,
        target_checkpoint.manifest().canonical().context().clone(),
        false,
    )?;
    let advanced_target = store
        .commit(CommitCheckpoint::new(
            target_lease.clone(),
            fork_receipt.target(),
            bundle(&advanced_target_checkpoint, Some(&target_bundle))?,
        ))
        .await?;
    store.release(target_lease).await?;
    assert_eq!(
        store.recover_fork(fork_recovery.clone()).await?,
        fork_receipt,
        "recovery validates the historical target checkpoint, not its advanced branch head"
    );
    assert_eq!(
        store
            .load_branch(SessionRef::new(session_id, target_branch))
            .await?
            .head(),
        advanced_target.head()
    );

    let foreign_owner = BranchLease::from_store(
        acquired.session(),
        owner,
        acquired.fence(),
        acquired.expires_at(),
        acquired.lease_seconds(),
    );
    let foreign_session = BranchLease::from_store(
        SessionRef::new(id::<SessionId>(22)?, acquired.session().branch_id()),
        acquired.owner(),
        acquired.fence(),
        acquired.expires_at(),
        acquired.lease_seconds(),
    );
    let foreign_branch = BranchLease::from_store(
        SessionRef::new(acquired.session().session_id(), id::<BranchId>(23)?),
        acquired.owner(),
        acquired.fence(),
        acquired.expires_at(),
        acquired.lease_seconds(),
    );
    let foreign_fence = BranchLease::from_store(
        acquired.session(),
        acquired.owner(),
        NonZeroU64::new(acquired.fence().get().checked_add(1).ok_or("lease fence")?)
            .ok_or("nonzero lease fence")?,
        acquired.expires_at(),
        acquired.lease_seconds(),
    );
    let foreign_duration = BranchLease::from_store(
        acquired.session(),
        acquired.owner(),
        acquired.fence(),
        acquired.expires_at(),
        NonZeroU64::new(
            acquired
                .lease_seconds()
                .get()
                .checked_add(1)
                .ok_or("lease duration")?,
        )
        .ok_or("nonzero lease duration")?,
    );
    for (token, expected) in [
        (foreign_owner, SessionStoreError::FenceSuperseded),
        (foreign_session, SessionStoreError::NotFound),
        (foreign_branch, SessionStoreError::NotFound),
        (foreign_fence, SessionStoreError::FenceSuperseded),
        (foreign_duration, SessionStoreError::FenceSuperseded),
    ] {
        assert_eq!(store.renew(token).await, Err(expected));
        assert_eq!(
            store.load_branch(source_ref).await?.head(),
            commit_receipt.head()
        );
        assert_eq!(store.acquire(acquisition).await?, acquired);
    }

    assert_eq!(store.renew(acquired.clone()).await?, acquired);
    clock.set(Timestamp::new(99, 0)?);
    assert_eq!(store.renew(acquired.clone()).await?, acquired);
    clock.set(Timestamp::new(101, 0)?);
    assert_eq!(
        store.lease_now()?,
        LeaseInstant::new(LeaseDomain::new([1; 16], [2; 32]), 101, 0)?
    );
    // The first successful reply is deliberately ignored to model acknowledgement loss.
    store.renew(acquired.clone()).await?;
    let renewed = store.renew(acquired.clone()).await?;
    assert_eq!(renewed, store.renew(acquired.clone()).await?);
    assert_eq!(
        store.release(acquired.clone()).await,
        Err(SessionStoreError::FenceSuperseded)
    );
    clock.set(Timestamp::new(111, 0)?);
    assert_eq!(
        store.lease_now()?,
        LeaseInstant::new(LeaseDomain::new([1; 16], [2; 32]), 111, 0)?
    );
    assert_eq!(
        store.renew(acquired.clone()).await,
        Err(SessionStoreError::LeaseExpired)
    );
    assert_eq!(
        store.renew(renewed).await,
        Err(SessionStoreError::LeaseExpired)
    );
    let after_expiry = store.acquire(acquisition).await?;
    assert!(after_expiry.fence() > acquired.fence());

    assert_eq!(
        store
            .archive(ArchiveSession::new(
                session_id,
                rebind_receipt.session().revision(),
            ))
            .await,
        Err(SessionStoreError::LeaseHeld)
    );
    store.release(after_expiry.clone()).await?;
    store.release(after_expiry).await?;
    let archive = ArchiveSession::new(session_id, rebind_receipt.session().revision());
    let archived = store.archive(archive).await?;
    assert_eq!(archived, store.archive(archive).await?);
    assert!(archived.session().is_archived());
    let listed = store
        .list(SessionQuery::new(NonZeroUsize::new(8).ok_or("nonzero page")?).include_archived(true))
        .await?;
    assert_eq!(listed.items().len(), 1);
    assert!(listed.items()[0].head().is_archived());
    let flush = store.flush().await?;
    assert!(flush.is_complete());
    assert_eq!(flush.effective(), expected_effective_durability);

    let shutdown = store.shutdown().await?;
    assert_eq!(shutdown.effective(), expected_effective_durability);
    assert_eq!(shutdown, store.shutdown().await?);
    for mode in [
        plakous_agent::internal::StoreReclaimMode::Bounded,
        plakous_agent::internal::StoreReclaimMode::Drain,
    ] {
        assert_eq!(store.reclaim(mode).await, Err(SessionStoreError::Shutdown));
    }
    assert_eq!(
        store.create(create.clone()).await,
        Err(SessionStoreError::Shutdown)
    );
    assert_eq!(store.commit(commit).await, Err(SessionStoreError::Shutdown));
    assert_eq!(
        store.fork(fork.clone()).await,
        Err(SessionStoreError::Shutdown)
    );
    assert_eq!(
        store.acquire(acquisition).await,
        Err(SessionStoreError::Shutdown)
    );
    assert_eq!(
        store
            .list(
                SessionQuery::new(NonZeroUsize::new(8).ok_or("nonzero page")?)
                    .include_archived(true),
            )
            .await,
        Err(SessionStoreError::Shutdown)
    );
    assert_eq!(
        store.archive(archive).await,
        Err(SessionStoreError::Shutdown)
    );
    assert_eq!(store.flush().await, Err(SessionStoreError::Shutdown));
    assert_eq!(created, store.recover_create(create).await?);
    assert_eq!(
        commit_receipt,
        store.recover_commit(commit_id).await?.receipt()
    );
    assert_eq!(
        store.recover_fork(fork_recovery).await,
        Err(SessionStoreError::FenceSuperseded)
    );
    assert_eq!(
        store.load_branch(source_ref).await,
        Err(SessionStoreError::Shutdown)
    );
    assert_eq!(
        store
            .load_checkpoint(commit_id)
            .await?
            .checkpoint()
            .as_ref(),
        &committed_checkpoint
    );
    assert!(
        !SessionStoreError::IdentityConflict
            .to_string()
            .contains(&session_id.to_string())
    );
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "the deletion conformance keeps one complete authority lifecycle auditable"
)]
pub(crate) async fn run_delete_conformance(
    store: &dyn SessionStore,
    durability: RuntimeDurability,
    base: u128,
) -> TestResult {
    let lease_seconds = NonZeroU64::new(10).ok_or("nonzero lease")?;
    let owner = id::<RunId>(base + 20)?;
    let mut created = Vec::new();
    for offset in [0_u128, 100] {
        let session_id = id::<SessionId>(base + offset)?;
        let branch_id = id::<BranchId>(base + offset + 1)?;
        let checkpoint_id = id::<CheckpointId>(base + offset + 2)?;
        let checkpoint = checkpoint(
            topology(session_id, branch_id, checkpoint_id, None, 0, None, 1)?,
            1,
            durability,
        )?;
        created.push(
            store
                .create(CreateSession::new(
                    SessionRef::new(session_id, branch_id),
                    Timestamp::new(50, 0)?,
                    owner,
                    lease_seconds,
                    bundle(&checkpoint, None)?,
                ))
                .await?,
        );
    }
    let target = &created[0];
    assert_eq!(
        store
            .delete(DeleteSession::new(
                target.session().session_id(),
                SessionRevision::INITIAL
            ))
            .await,
        Err(SessionStoreError::Invalid(
            StoreInvalidReason::SessionNotArchived
        ))
    );
    store.release(target.lease().clone()).await?;
    let archived = store
        .archive(ArchiveSession::new(
            target.session().session_id(),
            target.session().revision(),
        ))
        .await?;
    assert_eq!(
        store
            .delete(DeleteSession::new(
                target.session().session_id(),
                target.session().revision()
            ))
            .await,
        Err(SessionStoreError::Conflict)
    );
    let request = DeleteSession::new(target.session().session_id(), archived.session().revision());
    let receipt = store.delete(request).await?;
    assert_eq!(store.delete(request).await?, receipt);
    assert_eq!(store.recover_delete(request).await?, receipt);
    assert_eq!(
        store
            .load_checkpoint(target.branch().checkpoint().checkpoint_id())
            .await,
        Err(SessionStoreError::NotFound)
    );
    store
        .load_checkpoint(created[1].branch().checkpoint().checkpoint_id())
        .await?;
    assert_eq!(
        store
            .recover_create(CreateSession::new(
                target.branch().session(),
                Timestamp::new(50, 0)?,
                owner,
                lease_seconds,
                target_initial_bundle(target)?
            ))
            .await,
        Err(SessionStoreError::NotFound)
    );
    let recreated_checkpoint = checkpoint(
        topology(
            target.session().session_id(),
            target.session().initial_branch(),
            id::<CheckpointId>(base + 500)?,
            None,
            0,
            None,
            1,
        )?,
        1,
        durability,
    )?;
    assert_eq!(
        store
            .create(CreateSession::new(
                target.branch().session(),
                Timestamp::new(50, 0)?,
                owner,
                lease_seconds,
                bundle(&recreated_checkpoint, None)?
            ))
            .await,
        Err(SessionStoreError::IdentityConflict)
    );
    store.release(created[1].lease().clone()).await?;
    let last_archive = store
        .archive(ArchiveSession::new(
            created[1].session().session_id(),
            created[1].session().revision(),
        ))
        .await?;
    let last_request = DeleteSession::new(
        created[1].session().session_id(),
        last_archive.session().revision(),
    );
    let last_receipt = store.delete(last_request).await?;
    store
        .reclaim(plakous_agent::internal::StoreReclaimMode::Drain)
        .await?;
    assert_eq!(store.usage().await?.bounds()[1].used(), 0);
    assert_eq!(store.usage().await?.bounds()[2].used(), 0);
    store.shutdown().await?;
    assert_eq!(store.recover_delete(request).await?, receipt);
    assert_eq!(store.recover_delete(last_request).await?, last_receipt);
    Ok(())
}

fn target_initial_bundle(
    receipt: &plakous_agent::internal::CreateReceipt,
) -> TestResult<CheckpointObjectBundle> {
    let checkpoint = checkpoint(
        topology(
            receipt.session().session_id(),
            receipt.session().initial_branch(),
            receipt.branch().checkpoint().checkpoint_id(),
            None,
            0,
            None,
            1,
        )?,
        1,
        receipt.session().durability(),
    )?;
    bundle(&checkpoint, None)
}

/// One branch with `commits` successors of its create checkpoint, each
/// carrying distinct history so every pruned checkpoint frees objects.
pub(crate) struct PruneChain {
    pub(crate) created: plakous_agent::internal::CreateReceipt,
    pub(crate) ids: Vec<CheckpointId>,
    pub(crate) commits: Vec<CommitCheckpoint>,
    pub(crate) head: plakous_agent::internal::BranchHead,
    pub(crate) prior: CheckpointObjectBundle,
    pub(crate) last: RuntimeCheckpoint,
}

pub(crate) async fn prune_chain(
    store: &dyn SessionStore,
    durability: RuntimeDurability,
    base: u128,
    commits: u64,
    payload_bytes: usize,
) -> TestResult<PruneChain> {
    let session_id = id::<SessionId>(base)?;
    let branch_id = id::<BranchId>(base + 1)?;
    let selector = SessionRef::new(session_id, branch_id);
    let ids = (0..=commits)
        .map(|revision| id::<CheckpointId>(base + 100 + u128::from(revision)))
        .collect::<TestResult<Vec<_>>>()?;
    let genesis = checkpoint(
        topology(session_id, branch_id, ids[0], None, 0, None, 1)?,
        1,
        durability,
    )?;
    let prior = bundle(&genesis, None)?;
    let created = store
        .create(CreateSession::new(
            selector,
            Timestamp::new(50, 0)?,
            id::<RunId>(base + 2)?,
            NonZeroU64::new(1_000).ok_or("nonzero lease")?,
            prior.clone(),
        ))
        .await?;
    let mut chain = PruneChain {
        head: created.branch(),
        created,
        ids,
        commits: Vec::new(),
        prior,
        last: genesis,
    };
    for revision in 1..=commits {
        chain = extend_chain(store, durability, chain, revision, payload_bytes).await?;
    }
    Ok(chain)
}

/// Commits the successor at `revision`, whose id must already be in `ids`.
pub(crate) async fn extend_chain(
    store: &dyn SessionStore,
    durability: RuntimeDurability,
    mut chain: PruneChain,
    revision: u64,
    payload_bytes: usize,
) -> TestResult<PruneChain> {
    let selector = chain.created.branch().session();
    let index = usize::try_from(revision)?;
    let next = checkpoint_with_payload(
        topology(
            selector.session_id(),
            selector.branch_id(),
            chain.ids[index],
            Some(chain.ids[index - 1]),
            revision,
            None,
            chain.created.lease().fence().get(),
        )?,
        2,
        durability,
        RuntimeContextState::genesis(selector.session_id(), selector.branch_id(), [0x51; 32])?,
        false,
        &chain_payload(revision, payload_bytes),
    )?;
    let publication = bundle(&next, Some(&chain.prior))?;
    let request = CommitCheckpoint::new(
        chain.created.lease().clone(),
        chain.head,
        publication.clone(),
    );
    chain.head = store.commit(request.clone()).await?.head();
    chain.commits.push(request);
    chain.prior = publication;
    chain.last = next;
    Ok(chain)
}

pub(crate) fn chain_payload(revision: u64, payload_bytes: usize) -> String {
    format!(" {revision:08}").repeat(payload_bytes / 9 + 1)
}

/// Asserts which of `ids` load and which are typed as pruned.
async fn assert_retained(
    store: &dyn SessionStore,
    ids: &[CheckpointId],
    retained: &[usize],
) -> TestResult {
    for (index, checkpoint_id) in ids.iter().enumerate() {
        let loaded = store.load_checkpoint(*checkpoint_id).await;
        if retained.contains(&index) {
            let loaded = loaded.map_err(|error| format!("checkpoint {index}: {error}"))?;
            assert_eq!(
                loaded.bundle().validated_checkpoint()?.as_ref(),
                loaded.checkpoint().as_ref()
            );
        } else {
            assert_eq!(
                loaded.map(|_| ()),
                Err(SessionStoreError::Pruned),
                "checkpoint {index}"
            );
        }
    }
    Ok(())
}

/// History retention: the exact retained set, typed `Pruned` for removed
/// checkpoints and receipts, idempotent repeats, forks before and after a
/// prune, and exact usage accounting.
#[allow(
    clippy::too_many_lines,
    reason = "one ordered retention corpus is reused unchanged by every backend"
)]
pub(crate) async fn run_prune_conformance(
    store: &dyn SessionStore,
    durability: RuntimeDurability,
    base: u128,
) -> TestResult {
    const COMMITS: u64 = 12;
    let mut chain = prune_chain(store, durability, base, COMMITS, 512).await?;
    let lease = chain.created.lease().clone();
    let selector = chain.created.branch().session();

    let below_minimum = store
        .prune(PruneBranch::new(
            lease.clone(),
            chain.head,
            BTreeSet::new(),
            1_000,
        ))
        .await?;
    assert_eq!(below_minimum.pruned_checkpoints(), 0);
    assert_retained(store, &chain.ids, &(0..=12).collect::<Vec<_>>()).await?;
    assert_eq!(
        store
            .prune(PruneBranch::new(
                lease.clone(),
                chain.created.branch(),
                BTreeSet::new(),
                1,
            ))
            .await,
        Err(SessionStoreError::Conflict),
        "prune is bound to the exact head"
    );

    let before = store.usage().await?;
    let request = PruneBranch::new(lease.clone(), chain.head, BTreeSet::from([chain.ids[5]]), 1);
    let receipt = store.prune(request.clone()).await?;
    assert_eq!(receipt.head(), chain.head);
    assert_eq!(receipt.horizon().get(), COMMITS - 1);
    // Create checkpoint, the runtime-retained one, the head's parent, the head.
    let retained = [0, 5, 11, 12];
    assert_eq!(receipt.pruned_checkpoints(), COMMITS + 1 - 4);
    store
        .reclaim(plakous_agent::internal::StoreReclaimMode::Drain)
        .await?;
    assert_retained(store, &chain.ids, &retained).await?;
    let unknown = id::<CheckpointId>(base + 99)?;
    assert_eq!(
        store.load_checkpoint(unknown).await.map(|_| ()),
        Err(SessionStoreError::NotFound),
        "an unknown id is not a pruned checkpoint"
    );
    assert_eq!(
        store.recover_commit(unknown).await.map(|_| ()),
        Err(SessionStoreError::NotFound),
        "an unknown id is not a reclaimed receipt"
    );
    let after = store.usage().await?;
    let [checkpoints, objects, bytes] = after.bounds();
    let [before_checkpoints, before_objects, before_bytes] = before.bounds();
    assert_eq!(
        checkpoints.used(),
        before_checkpoints.used() - receipt.pruned_checkpoints()
    );
    assert!(objects.used() < before_objects.used());
    assert!(bytes.used() < before_bytes.used());

    // Receipts at or below the horizon are gone, typed; the head's stays.
    let head_receipt = store.recover_commit(chain.ids[12]).await?;
    assert_eq!(head_receipt.receipt().head(), chain.head);
    for pruned in [3, 5, 11] {
        assert_eq!(
            store.recover_commit(chain.ids[pruned]).await.map(|_| ()),
            Err(SessionStoreError::Pruned),
            "receipt {pruned}"
        );
    }
    assert_eq!(
        store.commit(chain.commits[2].clone()).await.map(|_| ()),
        Err(SessionStoreError::Pruned),
        "replaying a pruned commit is typed, never corrupt"
    );
    assert_eq!(
        store.commit(chain.commits[11].clone()).await?,
        head_receipt.receipt()
    );

    // Repeating the prune reclaims nothing more.
    let repeat = store.prune(request).await?;
    assert_eq!(
        (repeat.pruned_checkpoints(), repeat.horizon()),
        (0, receipt.horizon())
    );
    assert_eq!(store.usage().await?, after);
    assert_eq!(store.load_branch(selector).await?.head(), chain.head);

    // A fork from the pruned branch, then more history and a second prune:
    // the fork source survives it and the fork receipt replays exactly.
    chain.ids.extend(
        (COMMITS + 1..=COMMITS + 4)
            .map(|revision| id::<CheckpointId>(base + 100 + u128::from(revision)))
            .collect::<TestResult<Vec<_>>>()?,
    );
    chain = extend_chain(store, durability, chain, COMMITS + 1, 512).await?;
    let fork_source = chain.head;
    let target_branch = id::<BranchId>(base + 50)?;
    let fork_id = id::<CheckpointId>(base + 51)?;
    let fork = ordinary_fork(
        &chain,
        ForkTarget {
            branch: target_branch,
            checkpoint: fork_id,
            owner: id::<RunId>(base + 52)?,
            workflow: workflow(0xb2)?,
            session_revision: chain.created.session().revision(),
            source_revision: COMMITS + 1,
        },
        durability,
    )?;
    let fork_receipt = store.fork(fork.clone()).await?;
    for revision in COMMITS + 2..=COMMITS + 4 {
        chain = extend_chain(store, durability, chain, revision, 512).await?;
    }
    let second = store
        .prune(PruneBranch::new(
            lease.clone(),
            chain.head,
            BTreeSet::new(),
            1,
        ))
        .await?;
    assert_eq!(second.horizon().get(), COMMITS + 3);
    // 5 is no longer retained by the runtime; 13 is the fork source.
    assert_eq!(second.pruned_checkpoints(), 4);
    assert_retained(store, &chain.ids, &[0, 13, 15, 16]).await?;
    store.load_checkpoint(fork_id).await?;
    assert_eq!(store.fork(fork.clone()).await?, fork_receipt);
    assert_eq!(
        store
            .recover_fork(RecoverFork::new(
                fork.workflow_id(),
                fork.semantic_request(),
                lease.clone(),
                fork_source,
                target_branch,
            ))
            .await?,
        fork_receipt
    );

    // A fork from the pruned head works like any other.
    let late_target = id::<BranchId>(base + 60)?;
    let late_fork = ordinary_fork(
        &chain,
        ForkTarget {
            branch: late_target,
            checkpoint: id::<CheckpointId>(base + 61)?,
            owner: id::<RunId>(base + 62)?,
            workflow: workflow(0xb3)?,
            session_revision: fork_receipt.session().revision(),
            source_revision: COMMITS + 4,
        },
        durability,
    )?;
    let late = store.fork(late_fork).await?;
    assert_eq!(late.source(), chain.head);
    store.release(lease).await?;
    Ok(())
}

struct LostPruneAckStore {
    inner: Arc<dyn SessionStore>,
}

macro_rules! forward_store_calls {
    ($($name:ident($input:ty) -> $output:ty;)*) => {
        $(fn $name(&self, request: $input) -> plakous_agent::internal::StoreFuture<'_, $output> {
            self.inner.$name(request)
        })*
    };
}

impl SessionStore for LostPruneAckStore {
    fn lease_now(&self) -> Result<LeaseInstant, SessionStoreError> {
        self.inner.lease_now()
    }
    forward_store_calls! {
        create(CreateSession) -> plakous_agent::internal::CreateReceipt;
        recover_create(CreateSession) -> plakous_agent::internal::CreateReceipt;
        load_branch(SessionRef) -> plakous_agent::internal::LoadedBranch;
        load_checkpoint(CheckpointId) -> plakous_agent::internal::StoredCheckpoint;
        acquire(AcquireBranch) -> BranchLease;
        renew(BranchLease) -> BranchLease;
        release(BranchLease) -> ();
        commit(CommitCheckpoint) -> plakous_agent::internal::CommitReceipt;
        recover_commit(CheckpointId) -> plakous_agent::internal::CommittedCheckpoint;
        fork(ForkBranch) -> plakous_agent::internal::ForkReceipt;
        recover_fork(RecoverFork) -> plakous_agent::internal::ForkReceipt;
        list(SessionQuery) -> plakous_agent::internal::Page<plakous_agent::internal::SessionSummary>;
        archive(ArchiveSession) -> plakous_agent::internal::ArchiveReceipt;
        delete(DeleteSession) -> plakous_agent::internal::DeleteReceipt;
        recover_delete(DeleteSession) -> plakous_agent::internal::DeleteReceipt;
        feed_tail(SessionId) -> plakous_agent::internal::FeedTail;
    }
    fn read_feed(
        &self,
        session_id: SessionId,
        after: plakous_agent::internal::FeedSeq,
        max_records: NonZeroUsize,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::FeedPage> {
        self.inner.read_feed(session_id, after, max_records)
    }
    fn usage(
        &self,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::StoreUsage> {
        self.inner.usage()
    }
    fn flush(
        &self,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::DurabilityReport> {
        self.inner.flush()
    }
    fn shutdown(
        &self,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::DurabilityReport> {
        self.inner.shutdown()
    }
    fn prune(
        &self,
        request: PruneBranch,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::PruneReceipt> {
        Box::pin(async move {
            self.inner.prune(request).await?;
            Err(SessionStoreError::BackendUnavailable)
        })
    }
}

/// A live store reclaims a cached address, rejects its omission, and accepts
/// reconstruction of that exact address without reopening the backend.
pub(crate) async fn run_reclaimed_address_conformance(
    store: Arc<dyn SessionStore>,
    durability: RuntimeDurability,
    base: u128,
) -> TestResult {
    let mut chain = prune_chain(store.as_ref(), durability, base, 12, 512).await?;
    let selector = chain.head.session();
    assert_eq!(
        store
            .acquire(AcquireBranch::new(
                selector,
                id::<RunId>(base + 3)?,
                NonZeroU64::new(30).ok_or("lease duration")?,
            ))
            .await,
        Err(SessionStoreError::LeaseHeld),
        "an independent retention owner cannot acquire a live branch"
    );
    chain.ids.extend(
        (13..=30)
            .map(|revision| id::<CheckpointId>(base + 100 + revision))
            .collect::<TestResult<Vec<_>>>()?,
    );
    for _ in 0..4 {
        let before_prune = store.usage().await?;
        let lost_ack = LostPruneAckStore {
            inner: Arc::clone(&store),
        };
        assert_eq!(
            lost_ack
                .prune(PruneBranch::new(
                    chain.created.lease().clone(),
                    chain.head,
                    BTreeSet::new(),
                    1,
                ))
                .await,
            Err(SessionStoreError::BackendUnavailable)
        );
        store
            .reclaim(plakous_agent::internal::StoreReclaimMode::Drain)
            .await?;
        assert!(store.usage().await?.bounds()[1].used() < before_prune.bounds()[1].used());
        let revision = chain.head.revision().get() + 1;
        let next = checkpoint_with_payload(
            topology(
                selector.session_id(),
                selector.branch_id(),
                chain.ids[usize::try_from(revision)?],
                Some(chain.head.checkpoint().checkpoint_id()),
                revision,
                None,
                chain.created.lease().fence().get(),
            )?,
            2,
            durability,
            RuntimeContextState::genesis(selector.session_id(), selector.branch_id(), [0x51; 32])?,
            false,
            &chain_payload(1, 512),
        )?;
        let stale = bundle(&next, Some(&chain.prior))?;
        let before = store.usage().await?;
        let error = store
            .commit(CommitCheckpoint::new(
                chain.created.lease().clone(),
                chain.head,
                stale,
            ))
            .await
            .err()
            .ok_or("stale CAS membership must not omit a reclaimed child")?;
        assert_eq!(
            error,
            SessionStoreError::Corrupt(plakous_agent::internal::StoreIntegrityReason::MissingChild,)
        );
        assert_eq!(store.usage().await?, before);
        assert_eq!(store.load_branch(selector).await?.head(), chain.head);
        // Cold reconstruction of just the surviving head, not a backend reopen.
        // The owner's unconditional trim supplies this same reachable subset.
        chain.prior = store
            .load_checkpoint(chain.head.checkpoint().checkpoint_id())
            .await?
            .bundle()
            .clone();
        assert_head_closure(&chain.prior, &chain.last)?;
        let publication = bundle(&next, Some(&chain.prior))?;
        let request = CommitCheckpoint::new(
            chain.created.lease().clone(),
            chain.head,
            publication.clone(),
        );
        chain.head = store.commit(request).await?.head();
        chain.prior = publication;
        chain.last = next;
        store
            .load_checkpoint(chain.head.checkpoint().checkpoint_id())
            .await?;
        // Move both the head and its retained parent off the recreated address.
        for _ in 0..3 {
            let revision = chain.head.revision().get() + 1;
            chain = extend_chain(store.as_ref(), durability, chain, revision, 512).await?;
        }
    }
    store.release(chain.created.lease().clone()).await?;
    Ok(())
}

fn assert_head_closure(
    cached: &CheckpointObjectBundle,
    checkpoint: &RuntimeCheckpoint,
) -> TestResult {
    let closure = bundle(checkpoint, None)?;
    assert_eq!(
        cached.supplied_objects(),
        closure.supplied_objects(),
        "a surviving warm bundle contains only the head's closure"
    );
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) struct ForkTarget {
    pub(crate) branch: BranchId,
    pub(crate) checkpoint: CheckpointId,
    pub(crate) owner: RunId,
    pub(crate) workflow: SessionWorkflowId,
    pub(crate) session_revision: SessionRevision,
    pub(crate) source_revision: u64,
}

/// An ordinary fork of the chain's current head.
pub(crate) fn ordinary_fork(
    chain: &PruneChain,
    target: ForkTarget,
    durability: RuntimeDurability,
) -> TestResult<ForkBranch> {
    let source = chain.head;
    let session_id = source.session().session_id();
    let source_checkpoint = source.checkpoint().checkpoint_id();
    let lineage = ForkLineage {
        session_id,
        branch_id: source.session().branch_id(),
        checkpoint_id: source_checkpoint,
        branch_revision: source.revision().get(),
    };
    let context = chain.last.manifest().canonical().context().ordinary_fork(
        source_checkpoint,
        session_id,
        target.branch,
        [0x64; 32],
    )?;
    let checkpoint = checkpoint_with_payload(
        topology(
            session_id,
            target.branch,
            target.checkpoint,
            Some(source_checkpoint),
            0,
            Some(lineage),
            1,
        )?,
        2,
        durability,
        context,
        false,
        &chain_payload(target.source_revision, 512),
    )?;
    Ok(ForkBranch::new(
        target.workflow,
        semantic_request(b"ordinary fork")?,
        chain.created.lease().clone(),
        target.session_revision,
        source,
        target.branch,
        target.owner,
        NonZeroU64::new(1_000).ok_or("nonzero lease")?,
        bundle(&checkpoint, Some(&chain.prior))?,
    ))
}

#[tokio::test]
pub(super) async fn memory_semantic_deletion_conformance() -> TestResult {
    let store = MemorySessionStore::new(MemoryStoreLimits::default())?;
    run_delete_conformance(&store, RuntimeDurability::Disabled, 10_000).await?;
    let stats = store.stats()?;
    assert_eq!(stats.sessions(), 0);
    assert_eq!(stats.checkpoints(), 0);
    assert_eq!(stats.unique_objects(), 0);
    assert_eq!(stats.unique_object_bytes(), 0);
    assert_eq!(stats.tombstones(), 2);
    Ok(())
}

#[tokio::test]
pub(super) async fn memory_prune_conformance() -> TestResult {
    let store = Arc::new(MemorySessionStore::new(MemoryStoreLimits::default())?);
    run_prune_conformance(store.as_ref(), RuntimeDurability::Disabled, 30_000).await?;
    run_reclaimed_address_conformance(store, RuntimeDurability::Disabled, 35_000).await
}

#[tokio::test]
pub(super) async fn memory_backend_satisfies_semantic_conformance() -> TestResult {
    run_semantic_conformance(
        RuntimeDurability::Disabled,
        RuntimeDurability::Disabled,
        |clock| {
            Ok(Arc::new(MemorySessionStore::with_clock(
                MemoryStoreLimits::default(),
                clock,
            )?))
        },
    )
    .await
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one long-history scenario proves cache bounds, exact cold reconstruction, and incremental head reuse together"
)]
pub(super) async fn memory_retains_only_head_bundle_metadata_across_long_history() -> TestResult {
    const TURNS: u64 = 1_024;

    let clock = Arc::new(ManualLeaseClock::new(Timestamp::new(100, 0)?));
    let store = MemorySessionStore::with_clock(MemoryStoreLimits::default(), clock)?;
    let session_id = id::<SessionId>(100)?;
    let branch_id = id::<BranchId>(101)?;
    let owner = id::<RunId>(102)?;
    let lease_seconds = NonZeroU64::new(1_000).ok_or("nonzero lease")?;
    let selector = SessionRef::new(session_id, branch_id);
    let genesis_id = id::<CheckpointId>(1_000)?;
    let genesis = checkpoint(
        topology(session_id, branch_id, genesis_id, None, 0, None, 1)?,
        1,
        RuntimeDurability::Disabled,
    )?;
    let mut prior = bundle(&genesis, None)?;
    let mut published = BTreeSet::new();
    let mut published_bytes = 0_usize;
    for object in prior.supplied_objects() {
        assert!(published.insert(object.address()));
        published_bytes = published_bytes.saturating_add(object.retained_bytes());
    }
    let created = store
        .create(CreateSession::new(
            selector,
            Timestamp::new(50, 0)?,
            owner,
            lease_seconds,
            prior.clone(),
        ))
        .await?;
    let mut head = created.branch();
    let mut parent = genesis_id;
    let mut transferred_objects = prior.supplied_objects().len();
    let mut first_hot = None;

    for turn in 1..=TURNS {
        let checkpoint_id = id::<CheckpointId>(1_000 + u128::from(turn))?;
        let next = checkpoint(
            topology(
                session_id,
                branch_id,
                checkpoint_id,
                Some(parent),
                turn,
                None,
                1,
            )?,
            1,
            RuntimeDurability::Disabled,
        )?;
        let publication = bundle(&next, Some(&prior))?;
        assert_eq!(publication.base_root(), Some(prior.root()));
        assert!(publication.supplied_objects().len() <= 12);
        transferred_objects =
            transferred_objects.saturating_add(publication.supplied_objects().len());
        for object in publication.supplied_objects() {
            assert!(published.insert(object.address()));
            published_bytes = published_bytes.saturating_add(object.retained_bytes());
        }
        head = store
            .commit(CommitCheckpoint::new(
                created.lease().clone(),
                head,
                publication.clone(),
            ))
            .await?
            .head();
        if turn == 1 {
            first_hot = Some(store.load_checkpoint(checkpoint_id).await?);
        }
        prior = publication;
        parent = checkpoint_id;
    }

    let stats = store.stats()?;
    assert_eq!(stats.sessions(), 1);
    assert_eq!(stats.branches(), 1);
    assert_eq!(stats.checkpoints(), usize::try_from(TURNS + 1)?);
    assert_eq!(stats.unique_objects(), published.len());
    assert_eq!(stats.unique_object_bytes(), published_bytes);
    assert_eq!(stats.hot_bundles(), stats.branches());
    assert_eq!(stats.hot_bundles(), 1);
    assert_eq!(transferred_objects, published.len());
    assert!(transferred_objects <= usize::try_from(TURNS)?.saturating_mul(12) + 32);
    assert_eq!(head.revision().get(), TURNS);

    let loaded_head = store.load_branch(selector).await?;
    assert_eq!(loaded_head.head(), head);
    assert_eq!(
        loaded_head.checkpoint().bundle().base_root(),
        prior.base_root()
    );
    assert_eq!(
        loaded_head.checkpoint().bundle().supplied_objects().len(),
        prior.supplied_objects().len()
    );
    let successor_id = id::<CheckpointId>(3_000)?;
    let successor_checkpoint = checkpoint(
        topology(
            session_id,
            branch_id,
            successor_id,
            Some(parent),
            TURNS + 1,
            None,
            1,
        )?,
        1,
        RuntimeDurability::Disabled,
    )?;
    let successor = bundle(
        &successor_checkpoint,
        Some(loaded_head.checkpoint().bundle()),
    )?;
    assert_eq!(successor.base_root(), Some(head.checkpoint().root()));
    assert!(successor.supplied_objects().len() <= 12);

    let historical = store.load_checkpoint(id::<CheckpointId>(1_001)?).await?;
    let first_hot = first_hot.ok_or("first hot checkpoint")?;
    assert_eq!(historical, first_hot);
    assert_eq!(
        first_hot.bundle().base_root(),
        Some(created.branch().checkpoint().root())
    );
    assert_eq!(historical.bundle().base_root(), None);
    assert_eq!(
        historical.bundle().validated_checkpoint()?.as_ref(),
        historical.checkpoint().as_ref()
    );
    assert_eq!(store.stats()?.hot_bundles(), 1);
    store.release(created.lease().clone()).await?;
    store
        .archive(ArchiveSession::new(
            session_id,
            created.session().revision(),
        ))
        .await?;
    assert_eq!(store.stats()?.hot_bundles(), 0);
    assert_eq!(
        store.load_checkpoint(parent).await?.checkpoint().as_ref(),
        loaded_head.checkpoint().checkpoint().as_ref()
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn memory_enforces_named_session_and_page_bounds() -> TestResult {
    let one = NonZeroUsize::new(1).ok_or("nonzero bound")?;
    let defaults = MemoryStoreLimits::default();
    let limits = MemoryStoreLimits::new(
        one,
        defaults.max_branches_per_session(),
        defaults.max_checkpoints(),
        defaults.max_objects(),
        defaults.max_object_bytes(),
        one,
    );
    let store = MemorySessionStore::with_clock(
        limits,
        Arc::new(ManualLeaseClock::new(Timestamp::new(100, 0)?)),
    )?;
    let branch_id = id::<BranchId>(2_001)?;
    let owner = id::<RunId>(2_002)?;
    let lease_seconds = NonZeroU64::new(10).ok_or("nonzero lease")?;

    for ordinal in 0_u128..2 {
        let session_id = id::<SessionId>(2_100 + ordinal)?;
        let checkpoint_id = id::<CheckpointId>(2_200 + ordinal)?;
        let initial = checkpoint(
            topology(session_id, branch_id, checkpoint_id, None, 0, None, 1)?,
            1,
            RuntimeDurability::Disabled,
        )?;
        let result = store
            .create(CreateSession::new(
                SessionRef::new(session_id, branch_id),
                Timestamp::new(50, 0)?,
                owner,
                lease_seconds,
                bundle(&initial, None)?,
            ))
            .await;
        if ordinal == 0 {
            result?;
        } else {
            assert_eq!(
                result,
                Err(SessionStoreError::Capacity(SessionStoreCapacity {
                    bound: StoreCapacityBound::Sessions,
                    current: 1,
                    max: 1,
                    limit: 1,
                    admission: None,
                }))
            );
        }
    }

    assert_eq!(
        store
            .list(SessionQuery::new(
                NonZeroUsize::new(2).ok_or("nonzero page")?,
            ))
            .await,
        Err(SessionStoreError::Capacity(SessionStoreCapacity {
            bound: StoreCapacityBound::PageSize,
            current: 2,
            max: 1,
            limit: 1,
            admission: None,
        }))
    );
    Ok(())
}

/// Controllable domain and clock failures shared by backend authority tests.
pub(crate) struct DomainLeaseClock(Mutex<Result<LeaseInstant, SessionStoreError>>);

impl DomainLeaseClock {
    pub(crate) fn new(domain: LeaseDomain, seconds: u64) -> Result<Self, SessionStoreError> {
        Ok(Self(Mutex::new(Ok(LeaseInstant::new(domain, seconds, 0)?))))
    }

    pub(crate) fn set(&self, value: Result<LeaseInstant, SessionStoreError>) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = value;
    }
}

impl LeaseClock for DomainLeaseClock {
    fn now(&self) -> Result<LeaseInstant, SessionStoreError> {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

pub(crate) async fn run_expired_release_conformance(
    store: &dyn SessionStore,
    durability: RuntimeDurability,
    clock: &DomainLeaseClock,
) -> TestResult {
    for batched in [false, true] {
        let base = if batched { 110_000 } else { 100_000 };
        let first = prune_chain(store, durability, base, 0, 1).await?;
        let second = prune_chain(store, durability, base + 1_000, 0, 1).await?;
        let leases = vec![
            first.created.lease().clone(),
            second.created.lease().clone(),
        ];
        clock.set(Ok(leases[1].expires_at().checked_add_seconds(600)?));
        assert_eq!(
            store.renew(leases[0].clone()).await,
            Err(SessionStoreError::LeaseExpired)
        );
        for _ in 0..2 {
            if batched {
                store.release_many(leases.clone()).await?;
            } else {
                for lease in &leases {
                    store.release(lease.clone()).await?;
                }
            }
        }
        let acquire = |session, owner| {
            Ok::<_, Box<dyn Error>>(AcquireBranch::new(
                session,
                id::<RunId>(owner)?,
                NonZeroU64::new(30).ok_or("lease duration")?,
            ))
        };
        let prior = store
            .acquire(acquire(leases[0].session(), base + 5_000)?)
            .await?;
        clock.set(Ok(prior.expires_at().checked_add_seconds(600)?));
        let taken = store
            .acquire(acquire(prior.session(), base + 5_001)?)
            .await?;
        assert!(taken.fence() > prior.fence());
        assert_eq!(
            store.release(prior.clone()).await,
            Err(SessionStoreError::FenceSuperseded)
        );
        assert_eq!(
            store.release_many(vec![prior.clone()]).await,
            Err(SessionStoreError::FenceSuperseded)
        );

        let held = store
            .acquire(acquire(leases[1].session(), base + 5_002)?)
            .await?;
        clock.set(Ok(store.lease_now()?.checked_add_seconds(1)?));
        let renewed = store.renew(held.clone()).await?;
        assert_ne!(renewed, held);
        assert_eq!(
            store.release(held).await,
            Err(SessionStoreError::FenceSuperseded)
        );
        let expired = renewed.expires_at().checked_add_seconds(600)?;
        clock.set(Ok(LeaseInstant::new(
            LeaseDomain::new([3; 16], [4; 32]),
            expired.seconds(),
            0,
        )?));
        assert_eq!(
            store.release(renewed.clone()).await,
            Err(SessionStoreError::LeaseDomainMismatch)
        );
        clock.set(Ok(expired));
        if durability == RuntimeDurability::Required {
            // The valid expired grant must survive rollback of a stale peer.
            assert_eq!(
                store.release_many(vec![renewed.clone(), prior]).await,
                Err(SessionStoreError::FenceSuperseded)
            );
            assert_eq!(
                store.renew(renewed.clone()).await,
                Err(SessionStoreError::LeaseExpired)
            );
        }
        store.release_many(vec![renewed, taken]).await?;
    }
    Ok(())
}

#[tokio::test]
async fn memory_releases_expired_grants_without_releasing_replaced_grants() -> TestResult {
    let clock = Arc::new(DomainLeaseClock::new(
        LeaseDomain::new([1; 16], [2; 32]),
        100,
    )?);
    let store = MemorySessionStore::with_clock(MemoryStoreLimits::default(), clock.clone())?;
    run_expired_release_conformance(&store, RuntimeDurability::Disabled, &clock).await
}

#[tokio::test]
pub(super) async fn memory_lease_domain_and_clock_failure_refuse_without_publication() -> TestResult
{
    let domain = LeaseDomain::new([1; 16], [2; 32]);
    let clock = Arc::new(DomainLeaseClock::new(domain, 100)?);
    let store = MemorySessionStore::with_clock(MemoryStoreLimits::default(), clock.clone())?;
    let reference = SessionRef::new(id::<SessionId>(40_000)?, id::<BranchId>(40_001)?);
    let checkpoint = checkpoint(
        topology(
            reference.session_id(),
            reference.branch_id(),
            id::<CheckpointId>(40_002)?,
            None,
            0,
            None,
            1,
        )?,
        1,
        RuntimeDurability::Disabled,
    )?;
    let request = CreateSession::new(
        reference,
        Timestamp::new(50, 0)?,
        id::<RunId>(40_003)?,
        NonZeroU64::new(10).ok_or("duration")?,
        bundle(&checkpoint, None)?,
    );
    let created = store.create(request).await?;
    let before = store.stats()?;
    for failure in [
        Ok(LeaseInstant::new(
            LeaseDomain::new([1; 16], [3; 32]),
            100,
            0,
        )?),
        Ok(LeaseInstant::new(
            LeaseDomain::new([3; 16], [2; 32]),
            100,
            0,
        )?),
        Err(SessionStoreError::BackendUnavailable),
    ] {
        let expected = if failure.is_err() {
            SessionStoreError::BackendUnavailable
        } else {
            SessionStoreError::LeaseDomainMismatch
        };
        clock.set(failure);
        assert_eq!(store.lease_now(), Err(expected));
        assert_eq!(store.renew(created.lease().clone()).await, Err(expected));
        assert_eq!(store.stats()?, before);
    }
    clock.set(Ok(created.lease().expires_at()));
    assert_eq!(
        store.renew(created.lease().clone()).await,
        Err(SessionStoreError::LeaseExpired)
    );
    assert_eq!(store.stats()?, before);
    Ok(())
}

#[tokio::test]
pub(super) async fn memory_revives_an_expired_lease_only_when_nobody_took_it() -> TestResult {
    let domain = LeaseDomain::new([1; 16], [2; 32]);
    let clock = Arc::new(DomainLeaseClock::new(domain, 100)?);
    let store = MemorySessionStore::with_clock(MemoryStoreLimits::default(), clock.clone())?;
    let reference = SessionRef::new(id::<SessionId>(40_100)?, id::<BranchId>(40_101)?);
    let checkpoint = checkpoint(
        topology(
            reference.session_id(),
            reference.branch_id(),
            id::<CheckpointId>(40_102)?,
            None,
            0,
            None,
            1,
        )?,
        1,
        RuntimeDurability::Disabled,
    )?;
    let created = store
        .create(CreateSession::new(
            reference,
            Timestamp::new(50, 0)?,
            id::<RunId>(40_103)?,
            NonZeroU64::new(10).ok_or("duration")?,
            bundle(&checkpoint, None)?,
        ))
        .await?;
    let lease = created.lease().clone();
    clock.set(Ok(LeaseInstant::new(domain, 500, 0)?));
    assert_eq!(
        store.renew(lease.clone()).await,
        Err(SessionStoreError::LeaseExpired)
    );
    let revived = store.revive(lease.clone()).await?;
    assert_eq!(revived.fence(), lease.fence());
    assert_eq!(store.renew(revived.clone()).await?, revived);

    clock.set(Ok(LeaseInstant::new(domain, 1_000, 0)?));
    let taken = store
        .acquire(AcquireBranch::new(
            reference,
            id::<RunId>(40_104)?,
            NonZeroU64::new(10).ok_or("duration")?,
        ))
        .await?;
    assert!(taken.fence() > revived.fence());
    assert_eq!(
        store.revive(revived).await,
        Err(SessionStoreError::FenceSuperseded)
    );
    Ok(())
}

#[test]
pub(super) fn memory_construction_propagates_lease_clock_failure() -> TestResult {
    let clock = Arc::new(DomainLeaseClock::new(
        LeaseDomain::new([1; 16], [2; 32]),
        100,
    )?);
    clock.set(Err(SessionStoreError::BackendUnavailable));
    assert!(matches!(
        MemorySessionStore::with_clock(MemoryStoreLimits::default(), clock),
        Err(SessionStoreError::BackendUnavailable)
    ));
    Ok(())
}

/// One branch that publishes successors under a chosen admission class.
pub(crate) struct AdmissionChain {
    selector: SessionRef,
    lease: BranchLease,
    head: plakous_agent::internal::BranchHead,
    revision: u64,
    base: u128,
    prior: CheckpointObjectBundle,
    session_revision: SessionRevision,
    durability: RuntimeDurability,
}

impl AdmissionChain {
    pub(crate) async fn create(
        store: &dyn SessionStore,
        durability: RuntimeDurability,
        base: u128,
        admission: StoreAdmission,
    ) -> TestResult<Result<Self, SessionStoreError>> {
        let selector = SessionRef::new(id::<SessionId>(base)?, id::<BranchId>(base + 1)?);
        let genesis = checkpoint(
            topology(
                selector.session_id(),
                selector.branch_id(),
                id::<CheckpointId>(base + 2)?,
                None,
                0,
                None,
                1,
            )?,
            1,
            durability,
        )?;
        let prior = bundle(&genesis, None)?;
        let created = store
            .create(
                CreateSession::new(
                    selector,
                    Timestamp::new(50, 0)?,
                    id::<RunId>(base + 3)?,
                    NonZeroU64::new(3_600).ok_or("nonzero lease")?,
                    prior.clone(),
                )
                .with_admission(admission),
            )
            .await;
        Ok(created.map(|created| Self {
            selector,
            lease: created.lease().clone(),
            head: created.branch(),
            revision: 0,
            base,
            prior,
            session_revision: created.session().revision(),
            durability,
        }))
    }

    /// Publishes the next successor; a refusal leaves the chain unchanged.
    pub(crate) async fn commit(
        &mut self,
        store: &dyn SessionStore,
        admission: StoreAdmission,
    ) -> TestResult<Result<(), SessionStoreError>> {
        let revision = self.revision + 1;
        let next = checkpoint(
            topology(
                self.selector.session_id(),
                self.selector.branch_id(),
                id::<CheckpointId>(self.base + 1_000 + u128::from(revision))?,
                Some(self.head.checkpoint().checkpoint_id()),
                revision,
                None,
                1,
            )?,
            1,
            self.durability,
        )?;
        let publication = bundle(&next, Some(&self.prior))?;
        let committed = store
            .commit(
                CommitCheckpoint::new(self.lease.clone(), self.head, publication.clone())
                    .with_admission(admission),
            )
            .await;
        Ok(committed.map(|receipt| {
            self.head = receipt.head();
            self.revision = revision;
            self.prior = publication;
        }))
    }

    /// Releases, archives and deletes the whole session.
    pub(crate) async fn delete(self, store: &dyn SessionStore) -> TestResult {
        store.release(self.lease.clone()).await?;
        let archived = store
            .archive(ArchiveSession::new(
                self.selector.session_id(),
                self.session_revision,
            ))
            .await?;
        store
            .delete(DeleteSession::new(
                self.selector.session_id(),
                archived.session().revision(),
            ))
            .await?;
        Ok(())
    }
}

/// Checkpoint bound of the store `run_admission_conformance` expects: a
/// reserve of 2, so ordinary work stops at 14, settlement at 15, recovery at 16.
pub(crate) const ADMISSION_CHECKPOINTS: usize = 16;

/// Proves each admission class stops exactly at its threshold, that usage
/// reports it, and that deletion reclaims a full store.
pub(crate) async fn run_admission_conformance(
    store: &dyn SessionStore,
    durability: RuntimeDurability,
    base: u128,
) -> TestResult {
    let refused = |current, admission| {
        Err(SessionStoreError::admission_capacity(
            StoreCapacityBound::Checkpoints,
            current,
            16,
            admission,
        ))
    };
    let mut chain =
        AdmissionChain::create(store, durability, base, StoreAdmission::Ordinary).await??;
    for _ in 0..13 {
        chain.commit(store, StoreAdmission::Ordinary).await??;
    }
    assert_eq!(
        chain.commit(store, StoreAdmission::Ordinary).await?,
        refused(15, StoreAdmission::Ordinary)
    );
    let usage = store.usage().await?;
    let checkpoints = usage.bounds()[0];
    assert_eq!(checkpoints.bound(), StoreCapacityBound::Checkpoints);
    assert_eq!(
        (
            checkpoints.used(),
            checkpoints.limit(),
            checkpoints.reserve(),
            checkpoints.ordinary_threshold(),
            checkpoints.settlement_threshold(),
        ),
        (14, 16, 2, 14, 15)
    );
    assert_eq!(usage.ordinary_exhausted(), Some(checkpoints));
    assert!(matches!(
        AdmissionChain::create(store, durability, base + 500, StoreAdmission::Ordinary).await?,
        Err(SessionStoreError::Capacity(_))
    ));

    chain.commit(store, StoreAdmission::Settlement).await??;
    assert_eq!(
        chain.commit(store, StoreAdmission::Settlement).await?,
        refused(16, StoreAdmission::Settlement)
    );
    chain.commit(store, StoreAdmission::Recovery).await??;
    let over = chain.commit(store, StoreAdmission::Recovery).await?;
    assert_eq!(over, refused(17, StoreAdmission::Recovery));
    assert_eq!(
        over.map_err(|error| error.to_string()),
        Err("session store capacity was reached: checkpoints 17/16".to_owned())
    );
    assert_eq!(
        refused(15, StoreAdmission::Ordinary).map_err(|error: SessionStoreError| error.to_string()),
        Err::<(), _>(
            "session store capacity was reached: checkpoints 15/16 (ordinary writes stop at 14)"
                .to_owned()
        )
    );
    assert_eq!(store.usage().await?.bounds()[0].used(), 16);

    chain.delete(store).await?;
    assert_eq!(store.usage().await?.bounds()[0].used(), 0);
    assert_eq!(store.usage().await?.ordinary_exhausted(), None);
    AdmissionChain::create(store, durability, base + 600, StoreAdmission::Ordinary).await??;
    Ok(())
}

/// Small deterministic generator for the admission campaign.
struct Campaign(u64);

impl Campaign {
    fn next(&mut self, bound: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % bound
    }
}

#[cfg(test)]
fn checkpoint_limits(checkpoints: usize) -> TestResult<MemoryStoreLimits> {
    let defaults = MemoryStoreLimits::default();
    Ok(MemoryStoreLimits::new(
        defaults.max_sessions(),
        defaults.max_branches_per_session(),
        NonZeroUsize::new(checkpoints).ok_or("nonzero checkpoints")?,
        defaults.max_objects(),
        defaults.max_object_bytes(),
        defaults.max_page_size(),
    ))
}

#[tokio::test]
pub(super) async fn memory_admission_classes_stop_at_their_thresholds() -> TestResult {
    let clock = Arc::new(ManualLeaseClock::new(Timestamp::new(100, 0)?));
    let store = MemorySessionStore::with_clock(checkpoint_limits(ADMISSION_CHECKPOINTS)?, clock)?;
    run_admission_conformance(&store, RuntimeDurability::Disabled, 50_000).await
}

/// Random interleavings of agents that begin work until ordinary admission is
/// refused, then settle what they had in flight and recover. As long as the
/// settlement owed by in-flight work fits half the reserve, no settlement or
/// recovery commit is ever refused and the store never passes its limit.
#[tokio::test]
pub(super) async fn memory_admission_campaign_never_strands_settlement_or_recovery() -> TestResult {
    for seed in 1..=48_u64 {
        let mut random = Campaign(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let limit = 48 + random.next(80);
        let reserve = StoreAdmission::reserve(limit);
        let clock = Arc::new(ManualLeaseClock::new(Timestamp::new(100, 0)?));
        let store =
            MemorySessionStore::with_clock(checkpoint_limits(usize::try_from(limit)?)?, clock)?;
        let agents = 1 + random.next(3);
        let tail = (reserve / 2) / agents;
        let mut chains = Vec::new();
        for agent in 0..agents {
            let base = 60_000 + u128::from(seed) * 100_000 + u128::from(agent) * 10_000;
            chains.push(
                AdmissionChain::create(
                    &store,
                    RuntimeDurability::Disabled,
                    base,
                    StoreAdmission::Ordinary,
                )
                .await??,
            );
        }
        loop {
            let agent = usize::try_from(random.next(agents))?;
            match chains[agent]
                .commit(&store, StoreAdmission::Ordinary)
                .await?
            {
                Ok(()) => {}
                Err(SessionStoreError::Capacity(capacity)) => {
                    assert_eq!(capacity.admission, Some(StoreAdmission::Ordinary));
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }
        for chain in &mut chains {
            for _ in 0..=random.next(tail) {
                chain.commit(&store, StoreAdmission::Settlement).await??;
            }
        }
        for chain in &mut chains {
            chain.commit(&store, StoreAdmission::Recovery).await??;
        }
        let usage = store.usage().await?.bounds()[0];
        assert!(usage.used() <= usage.limit(), "seed {seed}");
        assert!(usage.ordinary_exhausted(), "seed {seed}");
    }
    Ok(())
}
