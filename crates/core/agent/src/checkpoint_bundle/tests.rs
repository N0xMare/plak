use super::*;
use crate::RuntimeComponentKind;
use crate::SessionWorkflowId;
use crate::manifest::{
    BindingReference, DurableCommandDisposition, DurableOperationKind, DurableOperationState,
    DurableOperationTransition, ModelRouteBinding, PROCESS_SESSION_ID_BYTES,
    PendingEffectDecisionState, ProcessSessionId, ProcessSessionOwnerGeneration,
    ProducerProvenance, RecoveryAction, RecoveryHandlerSignature, RecoveryObligation,
    RecoveryObligationKind, RevalidationFact, RuntimeDurability, RuntimeLifecycle, RuntimeManifest,
    RuntimeOperationRoot, RuntimeTopology, RuntimeWorkingRoot, ToolRuntimeManifest,
    WorkingRecoveryPhase, encode_bundle_operation_record,
};
use crate::{
    CompilerModelFacts, CompilerRuntimeFacts, CompilerToolFacts, FeatureSupportFact,
    HARNESS_PLAN_COMPONENT_FORMAT_VERSION, HarnessCompilerInputs, HarnessPlanComponentEnvelope,
    HarnessProfileCompiler, HarnessProfileId, HarnessProfileSpecBuilder,
};
use plakous_model::{
    Actor, BranchId, CachePolicy, CallId, CapabilitySet, ContentBlock, ContextAdmissionRequirement,
    ConversationItem, CredentialSelectionId, DataClassification, EffectiveProviderConfiguration,
    EntryId, FamilyPolicyEnvelope, GenerationLimits, HistorySegmentKind,
    LocalDurabilityDisposition, ModelAdapterProfileId, ModelFamilyId, ModelId,
    ModelImplementationIdentity, NativeOptions, OutputBoundDisposition, OutputPolicy,
    ParallelToolDisposition, ProtocolId, Provenance, ProviderAuthorityClassId,
    ProviderBehaviorConfiguration, ProviderProfileRevisionId, ProviderRouteSelection,
    ProviderSemanticEvidence, ProviderStorageDisposition, ReasoningBlock,
    ReasoningEffortDisposition, ReasoningIncludeDisposition, ReasoningPolicy,
    ReasoningSummaryDisposition, ReasoningVisibility, ReplayDisposition, ReplayEnvelope,
    ReplayKindId, ReplayScope, ResolvedModelDescriptor, ResolvedProviderAuthorityBinding,
    ResponseStateDisposition, RetentionDisposition, RouteId, RuntimeModelAuthority,
    SelectedProviderConfiguration, SessionId, TextBlock, Timestamp, ToolCallBlock, ToolCallState,
    ToolChoice, ToolCorrelation, ToolExecutionOwner, ToolName, ToolResultBlock, ToolResultKind,
    ToolResultPart, TurnId,
};
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn replace_supplied(bundle: &mut CheckpointObjectBundle, objects: Vec<RuntimeObject>) {
    let objects: Arc<[RuntimeObject]> = objects.into();
    bundle.objects = objects.clone();
    bundle.layers = Arc::new(ObjectLayer {
        prior: bundle.layers.prior.clone(),
        objects,
    });
}

fn id<T>(value: u128) -> TestResult<T>
where
    T: TryFrom<Uuid>,
    T::Error: std::error::Error + 'static,
{
    Ok(T::try_from(Uuid::from_u128(value))?)
}

fn digest(domain: DigestDomain, label: &[u8]) -> TestResult<Digest> {
    Ok(Digest::sha256(domain, 1, &[label])?)
}

fn component(
    kind: RuntimeComponentKind,
    revision: u32,
    label: &[u8],
) -> TestResult<ReconstructiveComponent> {
    Ok(ReconstructiveComponent::new(
        kind,
        revision,
        label.to_vec(),
    )?)
}

fn claude_thinking_replay() -> TestResult<ReplayEnvelope> {
    claude_thinking_replay_with_signature("claude-thinking-signature")
}

fn claude_thinking_replay_with_signature(signature: &str) -> TestResult<ReplayEnvelope> {
    let compiled = provider_configuration()?;
    // Frozen schema-3 payload: the checkpoint codec must preserve opaque bytes,
    // even when the owning provider no longer accepts this replay revision.
    // Current schema-4 replay compatibility is tested by the Messages adapters.
    let payload = serde_json::json!({
        "requested_model": "claude-sonnet-5",
        "actual_model": "claude-sonnet-5",
        "descriptor_binding": digest(DigestDomain::ModelBinding, b"claude-descriptor")?,
        "account_binding": digest(DigestDomain::Blob, b"claude-account")?,
        "selected_provider_configuration": compiled.selected_identity(),
        "effective_provider_configuration": compiled.effective_identity(),
        "replay_schema_revision": 3,
        "api_version": "2023-06-01",
        "beta_header": "oauth-2025-04-20,claude-code-20250219",
        "value": signature,
    });
    Ok(ReplayEnvelope::new(
        ProtocolId::new("anthropic.messages")?,
        None,
        ReplayScope::ExactModel(ModelId::new("claude-sonnet-5")?),
        ReplayKindId::new("messages.thinking_signature")?,
        3,
        DataClassification::Sensitive,
        serde_json::value::RawValue::from_string(payload.to_string())?,
    )?)
}

fn claude_shaped_tool_exchange() -> TestResult<HistorySegment> {
    claude_shaped_tool_exchange_with_thinking(
        ReasoningVisibility::Visible,
        Some("read the seed file".to_owned()),
    )
}

fn claude_shaped_empty_thinking_tool_exchange() -> TestResult<HistorySegment> {
    claude_shaped_tool_exchange_with_thinking(ReasoningVisibility::Omitted, None)
}

fn claude_shaped_tool_exchange_with_thinking(
    visibility: ReasoningVisibility,
    text: Option<String>,
) -> TestResult<HistorySegment> {
    let call_id = id::<CallId>(42)?;
    let correlation = ToolCorrelation::new(
        Some("toolu_01ReadSeed".to_owned()),
        None,
        plakous_model::ToolCallerLineage::Direct,
    )?;
    let assistant = ConversationItem::new(
        id::<EntryId>(50)?,
        Actor::Assistant,
        vec![
            ContentBlock::Reasoning(ReasoningBlock::new(
                visibility,
                text,
                Some(claude_thinking_replay()?),
            )?),
            ContentBlock::ToolCall(ToolCallBlock::new(
                call_id,
                ToolName::new("Read")?,
                ToolExecutionOwner::Local,
                ToolCallState::Requested,
                correlation.clone(),
                "{\"file_path\":\"SEED.txt\"}",
            )?),
        ],
        Provenance::ModelOutput,
    )?;
    let tool = ConversationItem::new(
        id::<EntryId>(51)?,
        Actor::Tool,
        vec![ContentBlock::ToolResult(ToolResultBlock::new(
            call_id,
            ToolExecutionOwner::Local,
            ToolResultKind::Success,
            vec![ToolResultPart::Text(TextBlock::new("seed-ok", None)?)],
            correlation,
        )?)],
        Provenance::ToolExecution,
    )?;
    Ok(HistorySegment::new(
        HistorySegmentKind::ToolExchange,
        vec![assistant, tool],
    )?)
}

pub(crate) fn history_segment(index: usize) -> TestResult<HistorySegment> {
    let item = ConversationItem::new(
        id::<EntryId>(10_000_u128.saturating_add(index as u128))?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new(
            format!("persistent turn {index}"),
            None,
        )?)],
        Provenance::UserInput,
    )?;
    Ok(HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![item],
    )?)
}

fn provider_configuration() -> TestResult<CompiledProviderConfiguration> {
    let descriptor = ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("test.protocol")?,
        RouteId::new("test.route")?,
        ModelId::new("test-model")?,
        None,
        Some(ModelFamilyId::new("bundle-family")?),
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
            revision: ProviderProfileRevisionId::new("bundle-retention-v1")?,
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
            ProviderProfileRevisionId::new("bundle-integration-v1")?,
            ModelAdapterProfileId::new("bundle.auth")?,
            ProviderProfileRevisionId::new("bundle-auth-v1")?,
            ProviderAuthorityClassId::new("bundle-test")?,
            CredentialSelectionId::new("bundle-test-credential")?,
            descriptor.requested_model().clone(),
        ),
        behavior.clone(),
    )?;
    let family_policy = FamilyPolicyEnvelope::new(
        descriptor.model_family().cloned().ok_or("model family")?,
        1,
        1,
        1,
        b"bundle-family-policy-v1".to_vec(),
    )?;
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        digest(DigestDomain::ModelBinding, b"bundle-account")?,
        digest(DigestDomain::ModelBinding, b"bundle-credential-authority")?,
        1,
        Timestamp::new(10, 0)?,
        Some(Timestamp::new(20, 0)?),
    )?;
    let stable_authority = ResolvedProviderAuthorityBinding::new(
        &descriptor,
        &authority,
        ProviderSemanticEvidence::new(1, b"bundle-provider-semantics-v1".to_vec())?,
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
        .id(HarnessProfileId::new("runtime.bundle.fixture")?)
        .base_instructions("exact runtime bundle fixture")
        .build()?;
    let descriptor = configuration.effective().descriptor();
    let model = CompilerModelFacts::new(
        configuration.selected_identity(),
        configuration.effective_identity(),
        descriptor.protocol().clone(),
        descriptor.route().clone(),
        descriptor.requested_model().clone(),
        descriptor.profile().clone(),
        descriptor.semantic_binding_digest()?,
        FeatureSupportFact::Unsupported,
        FeatureSupportFact::Unsupported,
        FeatureSupportFact::Unsupported,
    );
    let tools = CompilerToolFacts::new(
        digest(DigestDomain::ToolDefinition, b"bundle-tools")?,
        Vec::new(),
    )?;
    let runtime = CompilerRuntimeFacts::new(
        digest(DigestDomain::ExecutionEnvironment, b"bundle-environment")?,
        crate::MAX_INSTRUCTION_BYTES,
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
        crate::RUNTIME_POLICY_REVISION,
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

fn fixture(
    history_count: usize,
    checkpoint_number: u128,
    working: bool,
) -> TestResult<RuntimeCheckpoint> {
    let history = CanonicalHistory::new(
        (0..history_count)
            .map(history_segment)
            .collect::<Result<Vec<_>, _>>()?,
    )?;
    fixture_with_history(history, checkpoint_number, working)
}

pub(crate) fn fixture_with_history(
    history: CanonicalHistory,
    checkpoint_number: u128,
    working: bool,
) -> TestResult<RuntimeCheckpoint> {
    fixture_with_history_and_ledger(history, checkpoint_number, working, None, None)
}

pub(crate) fn fixture_with_history_and_ledger(
    history: CanonicalHistory,
    checkpoint_number: u128,
    working: bool,
    supplied_ledger: Option<RuntimeOperationLedger>,
    supplied_lifecycle: Option<RuntimeLifecycle>,
) -> TestResult<RuntimeCheckpoint> {
    fixture_with_context(
        history,
        checkpoint_number,
        working,
        supplied_ledger,
        supplied_lifecycle,
        RuntimeContextState::genesis(id(1)?, id(2)?, [0x4b; 32])?,
    )
}

fn fixture_with_context(
    history: CanonicalHistory,
    checkpoint_number: u128,
    working: bool,
    supplied_ledger: Option<RuntimeOperationLedger>,
    supplied_lifecycle: Option<RuntimeLifecycle>,
    context: RuntimeContextState,
) -> TestResult<RuntimeCheckpoint> {
    let provider_configuration = provider_configuration()?;
    let descriptor = provider_configuration.effective().descriptor();
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
    let canonical =
        RuntimeCanonicalState::new(prefix, history, WorkingProjection::default(), context)?;
    let account = BindingReference::new("account-authority", "account-ref", 1)?;
    let profile = BindingReference::new("profile-authority", "profile-ref", 1)?;
    let admission = RevalidationFact::new(
        account.clone(),
        Timestamp::new(10, 0)?,
        Some(Timestamp::new(20, 0)?),
        component(RuntimeComponentKind::AdmissionEvidence, 1, b"admission-v1")?,
    )?;
    let binding = ModelRouteBinding::new(
        descriptor.protocol().clone(),
        descriptor.route().clone(),
        descriptor.requested_model().clone(),
        descriptor.reported_model().cloned(),
        descriptor.model_family().cloned(),
        descriptor.profile().clone(),
        account,
        profile,
        descriptor.semantic_binding_digest()?,
        component(RuntimeComponentKind::ModelDescriptor, 1, b"descriptor-v1")?,
        admission,
    )?;
    let tools = ToolRuntimeManifest::new(
        component(
            RuntimeComponentKind::ToolRegistry,
            plakous_tools::TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION,
            b"registry-v1",
        )?,
        component(
            RuntimeComponentKind::ToolExposure,
            plakous_tools::TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION,
            b"exposure-v1",
        )?,
        component(
            RuntimeComponentKind::ExecutionEnvironment,
            1,
            b"environment-v1",
        )?,
        component(RuntimeComponentKind::EffectPolicy, 1, b"effect-policy-v1")?,
        BindingReference::new("policy-authority", "policy-ref", 1)?,
    )?;
    let turn_id = id::<TurnId>(7_001)?;
    let operation = DurableOperationRecord::new(
        1,
        DurableOperationKind::Provider {
            root: RuntimeOperationRoot::Turn(turn_id),
            attempt_id: id(7_002)?,
        },
        component(RuntimeComponentKind::Operation, 1, b"provider-request")?,
        if working {
            vec![DurableOperationTransition::new(
                id(7_003)?,
                DurableOperationState::Prepared,
            )]
        } else {
            vec![
                DurableOperationTransition::new(id(7_003)?, DurableOperationState::Prepared),
                DurableOperationTransition::new(
                    id(7_007)?,
                    DurableOperationState::LocatorBound(component(
                        RuntimeComponentKind::Operation,
                        1,
                        b"provider-locator",
                    )?),
                ),
                DurableOperationTransition::new(id(7_004)?, DurableOperationState::Dispatched),
                DurableOperationTransition::new(
                    id(7_005)?,
                    DurableOperationState::Completed(component(
                        RuntimeComponentKind::Operation,
                        1,
                        b"provider-outcome",
                    )?),
                ),
                DurableOperationTransition::new(
                    id(7_006)?,
                    DurableOperationState::Finalized(component(
                        RuntimeComponentKind::Operation,
                        1,
                        b"provider-outcome",
                    )?),
                ),
            ]
        },
    )?;
    let command = DurableCommandRecord::new(
        "command-1",
        1,
        Some(turn_id),
        digest(DigestDomain::Blob, b"command-binding")?,
        component(RuntimeComponentKind::Command, 1, b"command-input")?,
        DurableCommandDisposition::Accepted,
        None,
    )?;
    let ledger = match supplied_ledger {
        Some(ledger) => ledger,
        None => RuntimeOperationLedger::new(vec![command], vec![operation])?,
    };
    let lifecycle = if let Some(lifecycle) = supplied_lifecycle {
        lifecycle
    } else if working {
        let signature = RecoveryHandlerSignature::new(
            RecoveryObligationKind::ProviderAttempt,
            RecoveryAction::ProviderEvidenceRequired,
        );
        RuntimeLifecycle::Working {
            root: RuntimeWorkingRoot::Turn {
                turn_id,
                next_round: 1,
            },
            recovery_phase: WorkingRecoveryPhase::DispatchEvidence,
            obligations: vec![RecoveryObligation::new(1, signature)],
        }
    } else {
        RuntimeLifecycle::Quiescent
    };
    let artifact = FamilyArtifact::new(
        "responses",
        crate::manifest::RESPONSES_FAMILY_ARTIFACT_SCHEMA_REVISION,
        crate::manifest::RESPONSES_FAMILY_ARTIFACT_CODEC_REVISION,
        digest(DigestDomain::ModelBinding, b"family-binding-v1")?,
        b"opaque-family-artifact-v1".as_slice(),
    )?;
    let checkpoint_id = id::<CheckpointId>(20_000_u128.saturating_add(checkpoint_number))?;
    let manifest = RuntimeManifest::new(
        ProducerProvenance::new("plakous", "bundle-tests", "offline")?,
        RuntimeTopology {
            session_id: id::<SessionId>(1)?,
            branch_id: id::<BranchId>(2)?,
            checkpoint_id,
            parent_checkpoint_id: (checkpoint_number > 0)
                .then(|| id::<CheckpointId>(19_999_u128.saturating_add(checkpoint_number)))
                .transpose()?,
            branch_revision: u64::try_from(checkpoint_number)?,
            fork: None,
            lease_fence: 1,
            commit_identity: digest(
                DigestDomain::ProviderOperation,
                format!("commit-{checkpoint_number}").as_bytes(),
            )?,
        },
        lifecycle,
        harness_plan,
        provider_configuration,
        canonical,
        binding,
        tools,
        ledger,
        Some(artifact),
        None,
        crate::manifest::JobLedger::default(),
        RuntimeDurability::Required,
    )?;
    Ok(RuntimeCheckpoint::new(manifest)?)
}

fn compaction_working_fixture(checkpoint_number: u128) -> TestResult<RuntimeCheckpoint> {
    let workflow_id = SessionWorkflowId::new([0x5a; 32])?;
    let operation = DurableOperationRecord::new(
        1,
        DurableOperationKind::Compaction { workflow_id },
        component(RuntimeComponentKind::Operation, 1, b"compaction-request")?,
        vec![
            DurableOperationTransition::new(id(7_201)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(
                id(7_202)?,
                DurableOperationState::LocatorBound(component(
                    RuntimeComponentKind::Operation,
                    1,
                    b"compaction-locator",
                )?),
            ),
        ],
    )?;
    let commit = DurableOperationRecord::new(
        2,
        DurableOperationKind::Commit {
            root: RuntimeOperationRoot::SessionWorkflow(workflow_id),
        },
        component(RuntimeComponentKind::Operation, 1, b"commit-request")?,
        vec![
            DurableOperationTransition::new(id(7_101)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(id(7_102)?, DurableOperationState::Dispatched),
            DurableOperationTransition::new(
                id(7_103)?,
                DurableOperationState::Completed(component(
                    RuntimeComponentKind::Operation,
                    1,
                    b"commit-outcome",
                )?),
            ),
            DurableOperationTransition::new(
                id(7_104)?,
                DurableOperationState::Finalized(component(
                    RuntimeComponentKind::Operation,
                    1,
                    b"commit-outcome",
                )?),
            ),
        ],
    )?;
    let signature = RecoveryHandlerSignature::new(
        RecoveryObligationKind::Compaction,
        RecoveryAction::ProviderEvidenceRequired,
    );
    let lifecycle = RuntimeLifecycle::Working {
        root: RuntimeWorkingRoot::SessionWorkflow(workflow_id),
        recovery_phase: WorkingRecoveryPhase::DispatchEvidence,
        obligations: vec![RecoveryObligation::new(1, signature)],
    };
    let history =
        CanonicalHistory::new((0..3).map(history_segment).collect::<Result<Vec<_>, _>>()?)?;
    fixture_with_history_and_ledger(
        history,
        checkpoint_number,
        false,
        Some(RuntimeOperationLedger::new(
            Vec::new(),
            vec![operation, commit],
        )?),
        Some(lifecycle),
    )
}

#[test]
fn process_session_working_graph_materializes_exact_identity_and_obligation() -> TestResult {
    let turn_id = id::<TurnId>(7_001)?;
    let session_id = ProcessSessionId::new([0x71; PROCESS_SESSION_ID_BYTES])?;
    let owner_generation = ProcessSessionOwnerGeneration::new(17)?;
    let operation = DurableOperationRecord::new(
        1,
        DurableOperationKind::ProcessSession {
            turn_id,
            session_id,
            owner_generation,
        },
        component(
            RuntimeComponentKind::Operation,
            1,
            b"process-session-lifetime-v1",
        )?,
        vec![
            DurableOperationTransition::new(id(71_001)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(id(71_002)?, DurableOperationState::Dispatched),
        ],
    )?;
    let lifecycle = RuntimeLifecycle::Working {
        root: RuntimeWorkingRoot::Turn {
            turn_id,
            next_round: 2,
        },
        recovery_phase: WorkingRecoveryPhase::EffectReconciliation,
        obligations: vec![RecoveryObligation::new(
            1,
            RecoveryHandlerSignature::new(
                RecoveryObligationKind::ProcessSession,
                RecoveryAction::EffectEvidenceRequired,
            ),
        )],
    };
    let checkpoint = fixture_with_history_and_ledger(
        CanonicalHistory::new(vec![history_segment(0)?])?,
        71,
        true,
        Some(RuntimeOperationLedger::new(Vec::new(), vec![operation])?),
        Some(lifecycle),
    )?;
    let bundle = RuntimeCheckpointCodec::bundle(&checkpoint, None)?;
    let materialized = RuntimeCheckpointCodec::materialize(&bundle)?;
    assert_eq!(materialized, checkpoint);
    let operations = materialized.manifest().ledger().operations();
    let [record] = operations.as_slice() else {
        return Err("process-session graph changed operation cardinality".into());
    };
    assert!(matches!(
        record.kind(),
        DurableOperationKind::ProcessSession {
            turn_id: actual_turn,
            session_id: actual_session,
            owner_generation: actual_generation,
        } if *actual_turn == turn_id
            && *actual_session == session_id
            && *actual_generation == owner_generation
    ));
    assert_eq!(
        materialized.manifest().lifecycle().obligations()[0].kind(),
        RecoveryObligationKind::ProcessSession
    );
    Ok(())
}

#[test]
fn quiescent_and_working_materialize_exactly_from_the_graph_codec() -> TestResult {
    for checkpoint in [
        fixture(3, 3, false)?,
        fixture(3, 4, true)?,
        compaction_working_fixture(5)?,
    ] {
        let bundle = RuntimeCheckpointCodec::bundle(&checkpoint, None)?;
        let materialized = RuntimeCheckpointCodec::materialize(&bundle)?;
        assert_eq!(materialized, checkpoint);
        assert_eq!(materialized.integrity(), checkpoint.integrity());
        assert!(
            bundle
                .objects()
                .iter()
                .any(|object| { object.address().kind() == RuntimeObjectKind::FamilyArtifact })
        );
        assert_eq!(
            bundle
                .objects()
                .iter()
                .filter(|object| object.address().kind() == RuntimeObjectKind::HistorySegment)
                .count(),
            3
        );
    }
    Ok(())
}

/// A quiescent checkpoint whose context state records an occupancy anchor,
/// so the goldens pin the anchor's encoding as well as its absence.
fn anchored_quiescent_fixture() -> TestResult<RuntimeCheckpoint> {
    let history =
        CanonicalHistory::new((0..3).map(history_segment).collect::<Result<Vec<_>, _>>()?)?;
    let context = RuntimeContextState::genesis(id(1)?, id(2)?, [0x4b; 32])?
        .with_occupancy_anchor(plakous_model::ContextAnchor::new(298_468, 850_715))?;
    fixture_with_context(history, 3, false, None, None, context)
}

#[test]
fn v27_graph_goldens_pin_quiescent_and_working_closures() -> TestResult {
    let cases = [
        (
            fixture(3, 3, false)?,
            [
                185, 97, 150, 215, 154, 191, 178, 225, 181, 252, 39, 195, 251, 129, 25, 182, 100,
                121, 191, 178, 82, 41, 39, 80, 240, 68, 222, 8, 137, 62, 138, 100,
            ],
            [
                121, 12, 12, 111, 193, 210, 211, 139, 140, 150, 249, 23, 109, 48, 227, 29, 109,
                155, 166, 96, 195, 107, 21, 201, 206, 51, 133, 143, 70, 196, 117, 155,
            ],
            [
                202, 108, 9, 43, 195, 77, 17, 98, 214, 64, 98, 30, 224, 63, 200, 107, 203, 254,
                211, 174, 255, 221, 15, 236, 232, 198, 134, 149, 130, 29, 241, 177,
            ],
        ),
        (
            compaction_working_fixture(5)?,
            [
                176, 186, 10, 95, 252, 123, 234, 214, 255, 105, 203, 193, 130, 172, 168, 207, 247,
                70, 174, 185, 175, 216, 197, 231, 78, 246, 209, 93, 190, 133, 22, 134,
            ],
            [
                35, 104, 18, 42, 182, 172, 202, 33, 97, 215, 201, 178, 223, 128, 147, 242, 89, 93,
                79, 3, 230, 88, 161, 231, 49, 86, 98, 203, 236, 184, 231, 216,
            ],
            [
                181, 26, 22, 69, 59, 114, 150, 23, 42, 199, 0, 17, 12, 7, 14, 77, 17, 226, 183, 47,
                71, 149, 221, 28, 123, 139, 43, 73, 28, 216, 12, 196,
            ],
        ),
        (
            anchored_quiescent_fixture()?,
            [
                151, 116, 193, 114, 126, 236, 41, 115, 103, 98, 103, 64, 181, 79, 229, 214, 125,
                11, 181, 31, 61, 106, 170, 194, 156, 1, 249, 2, 156, 182, 150, 0,
            ],
            [
                89, 34, 145, 115, 190, 21, 214, 90, 239, 172, 25, 195, 207, 119, 84, 238, 77, 143,
                30, 9, 142, 113, 226, 121, 129, 168, 145, 230, 49, 63, 171, 105,
            ],
            [
                132, 36, 191, 206, 68, 11, 86, 47, 73, 251, 113, 56, 14, 131, 136, 138, 64, 17,
                181, 80, 248, 54, 140, 143, 242, 122, 87, 224, 43, 207, 101, 25,
            ],
        ),
    ];
    let mut actual = Vec::new();
    let mut expected = Vec::new();
    for (checkpoint, root, integrity, closure) in cases {
        let bundle = RuntimeCheckpointCodec::bundle(&checkpoint, None)?;
        assert_eq!(bundle.objects().len(), 34);
        let mut exact_closure = Vec::new();
        for object in bundle.objects() {
            exact_closure.push(object.address().kind() as u8);
            exact_closure.extend_from_slice(&object.address().codec_revision().to_be_bytes());
            exact_closure.extend_from_slice(object.address().digest().bytes());
            exact_closure
                .extend_from_slice(&u32::try_from(object.canonical_bytes().len())?.to_be_bytes());
            exact_closure.extend_from_slice(object.canonical_bytes());
            exact_closure.extend_from_slice(&u32::try_from(object.children().len())?.to_be_bytes());
            for child in object.children() {
                exact_closure.push(child.kind() as u8);
                exact_closure.extend_from_slice(&child.codec_revision().to_be_bytes());
                exact_closure.extend_from_slice(child.digest().bytes());
            }
        }
        let actual_closure = Digest::sha256(
            DigestDomain::Blob,
            RUNTIME_FORMAT_EPOCH,
            &[
                b"plakous-agent/runtime-checkpoint-exact-closure-golden-v1",
                &exact_closure,
            ],
        )?;
        actual.push((
            *bundle.root().digest().bytes(),
            *bundle.checkpoint_integrity().bytes(),
            *actual_closure.bytes(),
        ));
        expected.push((root, integrity, closure));
        assert_eq!(RuntimeCheckpointCodec::materialize(&bundle)?, checkpoint);
    }
    assert_eq!(actual, expected);
    Ok(())
}

#[test]
fn store_reassembly_is_complete_and_fail_closed() -> TestResult {
    let checkpoint = fixture(4, 4, false)?;
    let bundle = RuntimeCheckpointCodec::bundle(&checkpoint, None)?;
    let stored_objects = bundle
        .objects()
        .iter()
        .map(|object| {
            let kind = RuntimeObjectKind::from_store_tag(object.address().kind() as u8)?;
            let address = RuntimeObjectAddress::from_store_parts(
                kind,
                object.address().codec_revision(),
                object.address().digest(),
            )?;
            RuntimeObject::from_store(
                address,
                object.canonical_bytes().to_vec(),
                object.children().to_vec(),
            )
        })
        .collect::<Result<Vec<_>, CheckpointBundleError>>()?;
    let mirrored = bundle
        .objects()
        .iter()
        .find(|object| !object.children().is_empty())
        .expect("fixture has an object edge");
    assert_eq!(
        RuntimeObject::from_store(
            mirrored.address(),
            mirrored.canonical_bytes().to_vec(),
            Vec::new(),
        ),
        Err(CheckpointBundleError::NonCanonical)
    );
    let rebuilt = CheckpointObjectBundle::from_store(
        bundle.checkpoint_id(),
        bundle.checkpoint_integrity(),
        bundle.root(),
        stored_objects,
    )?;
    assert_eq!(RuntimeCheckpointCodec::materialize(&rebuilt)?, checkpoint);

    let mut missing = bundle.clone();
    let position = missing
        .objects
        .iter()
        .position(|object| object.address.kind == RuntimeObjectKind::HistorySegment)
        .expect("fixture has history");
    let mut objects = missing.objects.to_vec();
    objects.remove(position);
    replace_supplied(&mut missing, objects);
    assert!(matches!(
        RuntimeCheckpointCodec::materialize(&missing),
        Err(CheckpointBundleError::MissingObject)
    ));

    let mut reordered = bundle.clone();
    let mut objects = reordered.objects.to_vec();
    objects.swap(0, 1);
    replace_supplied(&mut reordered, objects);
    assert!(matches!(
        RuntimeCheckpointCodec::materialize(&reordered),
        Err(CheckpointBundleError::NonCanonical)
    ));

    let mut corrupt = bundle.clone();
    let mut objects = corrupt.objects.to_vec();
    let position = objects
        .iter()
        .position(|object| !object.canonical_bytes.is_empty())
        .expect("fixture has payloads");
    let mut bytes = objects[position].canonical_bytes.to_vec();
    bytes[0] ^= 1;
    objects[position].canonical_bytes = bytes.into();
    replace_supplied(&mut corrupt, objects);
    assert!(matches!(
        RuntimeCheckpointCodec::materialize(&corrupt),
        Err(CheckpointBundleError::InvalidAddress)
    ));

    for unsupported in [0, 3, u32::MAX] {
        let mut wrong_revision = bundle.clone();
        let mut objects = wrong_revision.objects.to_vec();
        objects[0].address.codec_revision = unsupported;
        objects.sort_by_key(RuntimeObject::address);
        replace_supplied(&mut wrong_revision, objects);
        assert_eq!(
            RuntimeCheckpointCodec::materialize(&wrong_revision),
            Err(CheckpointBundleError::UnsupportedRevision)
        );
    }

    let mut extra = bundle.clone();
    let extra_bytes = encode_bundle_component(&component(
        RuntimeComponentKind::ContextState,
        1,
        b"unreachable-valid-component",
    )?)?;
    let extra_address = object_address(RuntimeObjectKind::ReconstructiveComponent, &extra_bytes)?;
    let extra_object = RuntimeObject::from_store(extra_address, extra_bytes, Vec::new())?;
    let mut objects = extra.objects.to_vec();
    objects.push(extra_object);
    objects.sort_by_key(RuntimeObject::address);
    replace_supplied(&mut extra, objects);
    assert!(matches!(
        RuntimeCheckpointCodec::materialize(&extra),
        Err(CheckpointBundleError::ExtraObject)
    ));

    let mut wrong_children = bundle.clone();
    let mut objects = wrong_children.objects.to_vec();
    let position = objects
        .iter()
        .position(|object| !object.children.is_empty())
        .expect("fixture has edges");
    objects[position].children = Arc::new([]);
    replace_supplied(&mut wrong_children, objects);
    assert!(matches!(
        RuntimeCheckpointCodec::materialize(&wrong_children),
        Err(CheckpointBundleError::NonCanonical)
    ));
    let debug = format!("{bundle:?} {:?}", bundle.objects()[0]);
    assert!(!debug.contains("persistent turn"));
    assert!(!debug.contains("exact-harness-plan"));
    Ok(())
}

#[test]
fn long_operation_ledger_reopens_past_the_old_flat_manifest_graph_bound() -> TestResult {
    let turn_id = id::<TurnId>(7_001)?;
    let mut operations = Vec::new();
    for index in 1..=17_u64 {
        let request = component(
            RuntimeComponentKind::Operation,
            1,
            &vec![u8::try_from(index)?; 4_000_000],
        )?;
        operations.push(DurableOperationRecord::new(
            index,
            DurableOperationKind::Command {
                turn_id: Some(turn_id),
            },
            request,
            vec![DurableOperationTransition::new(
                id(20_000 + u128::from(index))?,
                DurableOperationState::Finalized(component(
                    RuntimeComponentKind::Operation,
                    1,
                    b"command-outcome",
                )?),
            )],
        )?);
    }
    let ledger = RuntimeOperationLedger::new(Vec::new(), operations)?;
    let checkpoint = fixture_with_history_and_ledger(
        CanonicalHistory::new(Vec::new())?,
        2,
        false,
        Some(ledger),
        None,
    )?;
    let bundle = RuntimeCheckpointCodec::bundle(&checkpoint, None)?;
    assert!(bundle.stats().retained_bytes() > MAX_RUNTIME_CHECKPOINT_BYTES * 4);
    let reopened = CheckpointObjectBundle::from_store(
        bundle.checkpoint_id(),
        bundle.checkpoint_integrity(),
        bundle.root(),
        bundle.objects().to_vec(),
    )?;
    assert_eq!(reopened.validated_checkpoint()?.as_ref(), &checkpoint);
    Ok(())
}

#[test]
fn incremental_publication_refuses_an_unreopenable_object_graph() -> TestResult {
    let history = CanonicalHistory::new(vec![history_segment(0)?])?;
    let mut prior =
        RuntimeCheckpointCodec::bundle(&fixture_with_history(history.clone(), 1, false)?, None)?;
    let next = fixture_with_history(history.append(history_segment(1)?)?, 2, false)?;
    prior.stats.retained_bytes = MAX_CHECKPOINT_OBJECT_BYTES;
    assert!(matches!(
        RuntimeCheckpointCodec::advance(&next, &prior),
        Err(CheckpointBundleError::BoundExceeded)
    ));
    assert_eq!(
        prior
            .validated_checkpoint()?
            .manifest()
            .topology()
            .branch_revision,
        1
    );
    Ok(())
}

#[test]
fn appended_history_reuses_prefix_and_unique_growth_is_linear() -> TestResult {
    let mut prior = None;
    let mut history = CanonicalHistory::new(Vec::new())?;
    let mut unique = BTreeMap::new();
    let mut cold_graph_bytes = 0_usize;
    let mut published_objects = 0_usize;
    let mut maximum_delta = 0_usize;
    let mut maximum_visited = 0_usize;
    let mut root_size = None;
    for turn in 1..=128_usize {
        history = history.append(history_segment(turn.saturating_sub(1))?)?;
        let checkpoint = fixture_with_history(history.clone(), turn as u128, false)?;
        cold_graph_bytes = cold_graph_bytes.saturating_add(
            RuntimeCheckpointCodec::bundle(&checkpoint, None)?
                .stats()
                .retained_bytes(),
        );
        let bundle = RuntimeCheckpointCodec::bundle(&checkpoint, prior.as_ref())?;
        maximum_delta = maximum_delta.max(bundle.stats().new_retained_bytes());
        if prior.is_some() {
            maximum_visited = maximum_visited.max(bundle.stats().visited_object_count());
        }
        published_objects = published_objects.saturating_add(bundle.supplied_objects().len());
        if prior.is_some() {
            assert!(bundle.stats().reused_object_count() >= turn.saturating_add(12));
            assert!(bundle.stats().new_object_count() <= 8);
            assert!(bundle.stats().visited_object_count() <= 20);
            assert!(
                bundle.stats().new_index_node_count()
                    <= bundle.supplied_objects().len().saturating_mul(297)
            );
        }
        let current_root_size = bundle
            .objects()
            .iter()
            .find(|object| object.address() == bundle.root())
            .expect("root belongs to closed set")
            .canonical_bytes()
            .len();
        assert_eq!(
            *root_size.get_or_insert(current_root_size),
            current_root_size
        );
        for object in bundle.objects() {
            unique
                .entry(object.address())
                .or_insert(object.retained_bytes());
        }
        prior = Some(bundle);
    }
    let unique_bytes = unique
        .values()
        .copied()
        .fold(0_usize, usize::saturating_add);
    assert!(unique_bytes.saturating_mul(3) < cold_graph_bytes);
    assert!(maximum_delta < 17 * 1024);
    assert!(maximum_visited <= 20);
    assert!(published_objects <= 128 * 8);
    assert_eq!(
        unique
            .keys()
            .filter(|address| address.kind() == RuntimeObjectKind::HistorySegment)
            .count(),
        128
    );
    assert_eq!(
        unique
            .keys()
            .filter(|address| address.kind() == RuntimeObjectKind::HistoryNode)
            .count(),
        128
    );
    Ok(())
}

#[test]
fn bounded_history_replacement_and_old_address_reversion_are_incremental() -> TestResult {
    let original_history = CanonicalHistory::new(vec![history_segment(0)?])?;
    let original_checkpoint = fixture_with_history(original_history.clone(), 1, false)?;
    let original = RuntimeCheckpointCodec::bundle(&original_checkpoint, None)?;

    let compacted_history = CanonicalHistory::new(vec![history_segment(1_000)?])?;
    let compacted_checkpoint = fixture_with_history(compacted_history, 2, false)?;
    let compacted = RuntimeCheckpointCodec::advance(&compacted_checkpoint, &original)?;
    assert!(compacted.stats().visited_object_count() <= 20);
    assert!(compacted.supplied_objects().len() <= 8);
    assert!(
        compacted.stats().new_index_node_count()
            <= compacted.supplied_objects().len().saturating_mul(297)
    );

    let reverted_history = CanonicalHistory::new(vec![history_segment(0)?])?;
    let reverted_checkpoint = fixture_with_history(reverted_history, 3, false)?;
    let reverted = RuntimeCheckpointCodec::advance(&reverted_checkpoint, &compacted)?;
    assert!(reverted.stats().visited_object_count() <= 20);
    assert!(
        reverted.stats().new_index_node_count()
            <= reverted.supplied_objects().len().saturating_mul(297)
    );
    assert!(reverted.supplied_objects().iter().all(|object| !matches!(
        object.address().kind(),
        RuntimeObjectKind::StablePrefix
            | RuntimeObjectKind::HistorySegment
            | RuntimeObjectKind::HistoryNode
            | RuntimeObjectKind::CanonicalState
    )));
    assert_eq!(
        RuntimeCheckpointCodec::materialize(&reverted)?,
        reverted_checkpoint
    );
    Ok(())
}

pub(crate) fn without_cached_objects(bundle: &CheckpointObjectBundle) -> CheckpointObjectBundle {
    let mut bundle = bundle.clone();
    bundle.cas = None;
    bundle
}

#[test]
fn current_root_closure_fails_closed_when_cached_objects_are_missing() -> TestResult {
    let checkpoint = fixture(1, 1, false)?;
    let bundle = without_cached_objects(&RuntimeCheckpointCodec::bundle(&checkpoint, None)?);
    assert!(matches!(
        bundle.current_root_closure(),
        Err(CheckpointBundleError::MissingObject)
    ));
    Ok(())
}

#[test]
fn claude_shaped_tool_exchange_with_visible_reasoning_advances() -> TestResult {
    let base_history = CanonicalHistory::new(vec![history_segment(0)?])?;
    let base = RuntimeCheckpointCodec::bundle(
        &fixture_with_history(base_history.clone(), 0, false)?,
        None,
    )?;
    let next_history = base_history.append(claude_shaped_tool_exchange()?)?;
    let next = fixture_with_history(next_history, 1, false)?;
    let advanced = RuntimeCheckpointCodec::advance(&next, &base)?;
    assert_ne!(advanced.supplied_objects(), []);
    assert_eq!(RuntimeCheckpointCodec::materialize(&advanced)?, next);
    Ok(())
}

#[test]
fn claude_empty_thinking_signature_tool_exchange_advances() -> TestResult {
    let base_history = CanonicalHistory::new(vec![history_segment(0)?])?;
    let base = RuntimeCheckpointCodec::bundle(
        &fixture_with_history(base_history.clone(), 0, false)?,
        None,
    )?;
    let next_history = base_history.append(claude_shaped_empty_thinking_tool_exchange()?)?;
    let next = fixture_with_history(next_history, 1, false)?;
    let advanced = RuntimeCheckpointCodec::advance(&next, &base)?;
    assert_eq!(RuntimeCheckpointCodec::materialize(&advanced)?, next);
    Ok(())
}

#[test]
fn claude_shaped_tool_exchange_advances_while_turn_is_working() -> TestResult {
    let base_history = CanonicalHistory::new(vec![history_segment(0)?])?;
    let base = RuntimeCheckpointCodec::bundle(
        &fixture_with_history(base_history.clone(), 0, true)?,
        None,
    )?;
    let next_history = base_history.append(claude_shaped_tool_exchange()?)?;
    let next = fixture_with_history(next_history, 1, true)?;
    let advanced = RuntimeCheckpointCodec::advance(&next, &base)?;
    assert_eq!(RuntimeCheckpointCodec::materialize(&advanced)?, next);
    Ok(())
}

#[test]
fn claude_working_dispatched_to_completed_then_tool_exchange_advances() -> TestResult {
    let base_history = CanonicalHistory::new(vec![history_segment(0)?])?;
    let prepared = fixture_with_history(base_history.clone(), 0, true)?;
    let prepared_bundle = RuntimeCheckpointCodec::bundle(&prepared, None)?;
    let dispatched_ledger = prepared.manifest().ledger().transition_operation(
        1,
        DurableOperationTransition::new(id(7_004)?, DurableOperationState::Dispatched),
    )?;
    let dispatched = fixture_with_history_and_ledger(
        base_history.clone(),
        1,
        true,
        Some(dispatched_ledger),
        None,
    )?;
    let dispatched_bundle = RuntimeCheckpointCodec::advance(&dispatched, &prepared_bundle)?;
    RuntimeCheckpointCodec::materialize(&dispatched_bundle)?;
    let completed_ledger = dispatched.manifest().ledger().transition_operation(
        1,
        DurableOperationTransition::new(
            id(7_005)?,
            DurableOperationState::Completed(component(
                RuntimeComponentKind::Operation,
                1,
                b"provider-completed-claude-round",
            )?),
        ),
    )?;
    let turn_id = id::<TurnId>(7_001)?;
    let signature = RecoveryHandlerSignature::new(
        RecoveryObligationKind::ProviderAttempt,
        RecoveryAction::FinalizationRequired,
    );
    let lifecycle = RuntimeLifecycle::Working {
        root: RuntimeWorkingRoot::Turn {
            turn_id,
            next_round: 1,
        },
        recovery_phase: WorkingRecoveryPhase::Finalization,
        obligations: vec![RecoveryObligation::new(1, signature)],
    };
    let next_history = base_history.append(claude_shaped_tool_exchange()?)?;
    let next = fixture_with_history_and_ledger(
        next_history,
        2,
        true,
        Some(completed_ledger),
        Some(lifecycle),
    )?;
    let advanced = RuntimeCheckpointCodec::advance(&next, &dispatched_bundle)
        .map_err(|error| format!("advance: {error}"))?;
    RuntimeCheckpointCodec::materialize(&advanced).map_err(|error| {
        format!(
            "materialize: {error}; supplied={:?}",
            advanced
                .supplied_objects()
                .iter()
                .map(|object| object.address().kind())
                .collect::<Vec<_>>()
        )
    })?;
    Ok(())
}

#[test]
fn claude_messages_large_thinking_tool_exchange_advances() -> TestResult {
    // Messages thinking is a JSON string, so stay under SemanticJsonV1's
    // 256 KiB string bound while exceeding a small-envelope guess.
    let thinking = "t".repeat(200_000);
    let call_id = id::<CallId>(42)?;
    let correlation = ToolCorrelation::new(
        Some("toolu_01ReadSeed".to_owned()),
        None,
        plakous_model::ToolCallerLineage::Direct,
    )?;
    let segment = HistorySegment::new(
        HistorySegmentKind::ToolExchange,
        vec![
            ConversationItem::new(
                id::<EntryId>(50)?,
                Actor::Assistant,
                vec![
                    ContentBlock::Reasoning(ReasoningBlock::new(
                        ReasoningVisibility::Visible,
                        Some(thinking),
                        Some(claude_thinking_replay()?),
                    )?),
                    ContentBlock::ToolCall(ToolCallBlock::new(
                        call_id,
                        ToolName::new("Read")?,
                        ToolExecutionOwner::Local,
                        ToolCallState::Requested,
                        correlation.clone(),
                        "{\"file_path\":\"SEED.txt\"}",
                    )?),
                ],
                Provenance::ModelOutput,
            )?,
            ConversationItem::new(
                id::<EntryId>(51)?,
                Actor::Tool,
                vec![ContentBlock::ToolResult(ToolResultBlock::new(
                    call_id,
                    ToolExecutionOwner::Local,
                    ToolResultKind::Success,
                    vec![ToolResultPart::Text(TextBlock::new("seed-ok", None)?)],
                    correlation,
                )?)],
                Provenance::ToolExecution,
            )?,
        ],
    )?;
    assert!(segment.canonical_bytes().len() > 180_000);
    encode_bundle_history_segment(&segment)?;
    let base_history = CanonicalHistory::new(vec![history_segment(0)?])?;
    let base = RuntimeCheckpointCodec::bundle(
        &fixture_with_history(base_history.clone(), 0, false)?,
        None,
    )?;
    let next_history = base_history.append(segment)?;
    let next = fixture_with_history(next_history, 1, false)?;
    let advanced = RuntimeCheckpointCodec::advance(&next, &base)?;
    assert_eq!(RuntimeCheckpointCodec::materialize(&advanced)?, next);
    Ok(())
}

#[test]
fn claude_read_edit_then_quiescent_final_text_advances() -> TestResult {
    let mut history = CanonicalHistory::new(vec![history_segment(0)?])?;
    let mut bundle =
        RuntimeCheckpointCodec::bundle(&fixture_with_history(history.clone(), 0, true)?, None)?;
    history = history.append(claude_shaped_tool_exchange()?)?;
    let after_read = fixture_with_history(history.clone(), 1, true)?;
    bundle = RuntimeCheckpointCodec::advance(&after_read, &bundle)
        .map_err(|error| format!("read exchange: {error}"))?;
    RuntimeCheckpointCodec::materialize(&bundle)?;

    let edit_call = id::<CallId>(43)?;
    let edit_correlation = ToolCorrelation::new(
        Some("toolu_01EditSeed".to_owned()),
        None,
        plakous_model::ToolCallerLineage::Direct,
    )?;
    let edit = HistorySegment::new(
        HistorySegmentKind::ToolExchange,
        vec![
            ConversationItem::new(
                id::<EntryId>(60)?,
                Actor::Assistant,
                vec![
                    ContentBlock::Reasoning(ReasoningBlock::new(
                        ReasoningVisibility::Visible,
                        Some("edit the seed file".to_owned()),
                        Some(claude_thinking_replay_with_signature("edit-sig")?),
                    )?),
                    ContentBlock::ToolCall(ToolCallBlock::new(
                        edit_call,
                        ToolName::new("Edit")?,
                        ToolExecutionOwner::Local,
                        ToolCallState::Requested,
                        edit_correlation.clone(),
                        "{\"file_path\":\"SEED.txt\",\"old_string\":\"seed-ok\",\"new_string\":\"edited\"}",
                    )?),
                ],
                Provenance::ModelOutput,
            )?,
            ConversationItem::new(
                id::<EntryId>(61)?,
                Actor::Tool,
                vec![ContentBlock::ToolResult(ToolResultBlock::new(
                    edit_call,
                    ToolExecutionOwner::Local,
                    ToolResultKind::Success,
                    vec![ToolResultPart::Text(TextBlock::new(
                        "SEED.txt updated",
                        None,
                    )?)],
                    edit_correlation,
                )?)],
                Provenance::ToolExecution,
            )?,
        ],
    )?;
    history = history.append(edit)?;
    let after_edit = fixture_with_history(history.clone(), 2, true)?;
    bundle = RuntimeCheckpointCodec::advance(&after_edit, &bundle)
        .map_err(|error| format!("edit exchange: {error}"))?;
    RuntimeCheckpointCodec::materialize(&bundle)?;

    let dispatched_ledger = after_edit.manifest().ledger().transition_operation(
        1,
        DurableOperationTransition::new(id(7_004)?, DurableOperationState::Dispatched),
    )?;
    let dispatched =
        fixture_with_history_and_ledger(history.clone(), 3, true, Some(dispatched_ledger), None)?;
    bundle = RuntimeCheckpointCodec::advance(&dispatched, &bundle)
        .map_err(|error| format!("dispatched: {error}"))?;

    let completed_ledger = dispatched.manifest().ledger().transition_operation(
        1,
        DurableOperationTransition::new(
            id(7_005)?,
            DurableOperationState::Completed(component(
                RuntimeComponentKind::Operation,
                1,
                b"provider-completed-claude-final",
            )?),
        ),
    )?;
    let final_segment = HistorySegment::new(
        HistorySegmentKind::ReasoningReplay,
        vec![ConversationItem::new(
            id::<EntryId>(70)?,
            Actor::Assistant,
            vec![
                ContentBlock::Reasoning(ReasoningBlock::new(
                    ReasoningVisibility::Visible,
                    Some("report the edited marker".to_owned()),
                    Some(claude_thinking_replay_with_signature("final-sig")?),
                )?),
                ContentBlock::Text(TextBlock::new("printed=edited", None)?),
            ],
            Provenance::ModelOutput,
        )?],
    )?;
    history = history.append(final_segment)?;
    let turn_id = id::<TurnId>(7_001)?;
    let signature = RecoveryHandlerSignature::new(
        RecoveryObligationKind::ProviderAttempt,
        RecoveryAction::FinalizationRequired,
    );
    let completing = RuntimeLifecycle::Working {
        root: RuntimeWorkingRoot::Turn {
            turn_id,
            next_round: 1,
        },
        recovery_phase: WorkingRecoveryPhase::Finalization,
        obligations: vec![RecoveryObligation::new(1, signature)],
    };
    let completed = fixture_with_history_and_ledger(
        history.clone(),
        4,
        true,
        Some(completed_ledger),
        Some(completing),
    )?;
    bundle = RuntimeCheckpointCodec::advance(&completed, &bundle)
        .map_err(|error| format!("completed with final text: {error}"))?;
    RuntimeCheckpointCodec::materialize(&bundle)?;

    let finalized_ledger = completed.manifest().ledger().transition_operation(
        1,
        DurableOperationTransition::new(
            id(7_006)?,
            DurableOperationState::Finalized(component(
                RuntimeComponentKind::Operation,
                1,
                b"provider-completed-claude-final",
            )?),
        ),
    )?;
    let quiescent = fixture_with_history_and_ledger(
        history,
        5,
        false,
        Some(finalized_ledger),
        Some(RuntimeLifecycle::Quiescent),
    )?;
    let advanced = RuntimeCheckpointCodec::advance(&quiescent, &bundle)
        .map_err(|error| format!("quiescent final: {error}"))?;
    assert_eq!(RuntimeCheckpointCodec::materialize(&advanced)?, quiescent);
    Ok(())
}

#[test]
fn claude_live_promote_final_text_history_and_quiescent_in_one_advance() -> TestResult {
    let mut history = CanonicalHistory::new(vec![history_segment(0)?])?;
    let mut bundle =
        RuntimeCheckpointCodec::bundle(&fixture_with_history(history.clone(), 0, true)?, None)?;
    history = history.append(claude_shaped_tool_exchange()?)?;
    let after_read = fixture_with_history(history.clone(), 1, true)?;
    bundle = RuntimeCheckpointCodec::advance(&after_read, &bundle)?;

    let dispatched_ledger = after_read.manifest().ledger().transition_operation(
        1,
        DurableOperationTransition::new(id(7_004)?, DurableOperationState::Dispatched),
    )?;
    let dispatched =
        fixture_with_history_and_ledger(history.clone(), 2, true, Some(dispatched_ledger), None)?;
    bundle = RuntimeCheckpointCodec::advance(&dispatched, &bundle)
        .map_err(|error| format!("dispatched: {error}"))?;

    let completed_ledger = dispatched.manifest().ledger().transition_operation(
        1,
        DurableOperationTransition::new(
            id(7_005)?,
            DurableOperationState::Completed(component(
                RuntimeComponentKind::Operation,
                1,
                b"provider-pending-terminal",
            )?),
        ),
    )?;
    let turn_id = id::<TurnId>(7_001)?;
    let signature = RecoveryHandlerSignature::new(
        RecoveryObligationKind::ProviderAttempt,
        RecoveryAction::FinalizationRequired,
    );
    let pending = RuntimeLifecycle::Working {
        root: RuntimeWorkingRoot::Turn {
            turn_id,
            next_round: 1,
        },
        recovery_phase: WorkingRecoveryPhase::Finalization,
        obligations: vec![RecoveryObligation::new(1, signature)],
    };
    let pending_checkpoint = fixture_with_history_and_ledger(
        history.clone(),
        3,
        true,
        Some(completed_ledger),
        Some(pending),
    )?;
    bundle = RuntimeCheckpointCodec::advance(&pending_checkpoint, &bundle)
        .map_err(|error| format!("pending terminal: {error}"))?;

    let final_segment = HistorySegment::new(
        HistorySegmentKind::ReasoningReplay,
        vec![ConversationItem::new(
            id::<EntryId>(80)?,
            Actor::Assistant,
            vec![
                ContentBlock::Reasoning(ReasoningBlock::new(
                    ReasoningVisibility::Visible,
                    Some("report the edited marker".to_owned()),
                    Some(claude_thinking_replay_with_signature("promote-sig")?),
                )?),
                ContentBlock::Text(TextBlock::new("printed=edited", None)?),
            ],
            Provenance::ModelOutput,
        )?],
    )?;
    history = history.append(final_segment)?;
    let promoted_ledger = pending_checkpoint
        .manifest()
        .ledger()
        .transition_operation(
            1,
            DurableOperationTransition::new(
                id(7_006)?,
                DurableOperationState::Finalized(component(
                    RuntimeComponentKind::Operation,
                    1,
                    b"provider-pending-terminal",
                )?),
            ),
        )?;
    let promoted = fixture_with_history_and_ledger(
        history,
        4,
        false,
        Some(promoted_ledger),
        Some(RuntimeLifecycle::Quiescent),
    )?;
    let advanced = RuntimeCheckpointCodec::advance(&promoted, &bundle)
        .map_err(|error| format!("live promote history+Finalized+Quiescent: {error}"))?;
    assert_eq!(RuntimeCheckpointCodec::materialize(&advanced)?, promoted);
    Ok(())
}

#[test]
fn append_shared_ledger_publication_work_and_transfer_are_log_bounded() -> TestResult {
    let history = CanonicalHistory::new(vec![history_segment(0)?])?;
    let mut ledger = RuntimeOperationLedger::new(Vec::new(), Vec::new())?;
    let mut prior = None;
    let mut published_objects = 0_usize;
    let mut maximum_supplied = (0_usize, 0_u64);
    let mut maximum_visited = (0_usize, 0_u64);
    let mut maximum_index_nodes = (0_usize, 0_u64);
    for turn in 1..=128_u64 {
        let command_label = format!("command-{turn}");
        ledger = ledger
            .append_command(DurableCommandRecord::new(
                command_label.clone(),
                turn,
                None,
                digest(DigestDomain::Blob, command_label.as_bytes())?,
                component(RuntimeComponentKind::Command, 1, command_label.as_bytes())?,
                DurableCommandDisposition::Accepted,
                None,
            )?)
            .map_err(|error| format!("command turn {turn}: {error:?}"))?;
        let request_label = format!("operation-{turn}");
        let transition_base = 50_000_u128.saturating_add(u128::from(turn).saturating_mul(3));
        ledger = ledger
            .append_operation(DurableOperationRecord::new(
                turn,
                DurableOperationKind::Command { turn_id: None },
                component(RuntimeComponentKind::Operation, 1, request_label.as_bytes())?,
                vec![
                    DurableOperationTransition::new(
                        id(transition_base)?,
                        DurableOperationState::Prepared,
                    ),
                    DurableOperationTransition::new(
                        id(transition_base.saturating_add(1))?,
                        DurableOperationState::ProvenNotDispatched(component(
                            RuntimeComponentKind::Operation,
                            1,
                            format!("proof-{turn}").as_bytes(),
                        )?),
                    ),
                    DurableOperationTransition::new(
                        id(transition_base.saturating_add(2))?,
                        DurableOperationState::Finalized(component(
                            RuntimeComponentKind::Operation,
                            1,
                            format!("proof-{turn}").as_bytes(),
                        )?),
                    ),
                ],
            )?)
            .map_err(|error| format!("operation turn {turn}: {error:?}"))?;
        let checkpoint = fixture_with_history_and_ledger(
            history.clone(),
            u128::from(turn),
            false,
            Some(ledger.clone()),
            None,
        )
        .map_err(|error| format!("checkpoint turn {turn}: {error}"))?;
        let bundle = RuntimeCheckpointCodec::bundle(&checkpoint, prior.as_ref())
            .map_err(|error| format!("ledger turn {turn}: {error:?}"))?;
        published_objects = published_objects.saturating_add(bundle.supplied_objects().len());
        if prior.is_some() {
            if bundle.supplied_objects().len() > maximum_supplied.0 {
                maximum_supplied = (bundle.supplied_objects().len(), turn);
            }
            if bundle.stats().visited_object_count() > maximum_visited.0 {
                maximum_visited = (bundle.stats().visited_object_count(), turn);
            }
            if bundle.stats().new_index_node_count() > maximum_index_nodes.0 {
                maximum_index_nodes = (bundle.stats().new_index_node_count(), turn);
            }
            assert!(
                bundle.stats().new_index_node_count()
                    <= bundle.supplied_objects().len().saturating_mul(297)
            );
        }
        prior = Some(bundle);
    }
    let final_bundle = prior.ok_or("ledger fixture produced no checkpoint")?;
    let final_object_count = final_bundle.stats().object_count();
    let final_retained_bytes = final_bundle.stats().retained_bytes();
    let materialized = RuntimeCheckpointCodec::materialize(&final_bundle).map_err(|error| {
        format!("materialize {final_object_count} objects/{final_retained_bytes} bytes: {error:?}")
    })?;
    assert_eq!(materialized.manifest().ledger(), &ledger);
    assert!(maximum_supplied.0 <= 29, "{maximum_supplied:?}");
    assert!(maximum_visited.0 <= 29, "{maximum_visited:?}");
    assert!(maximum_index_nodes.0 <= 18 * 297, "{maximum_index_nodes:?}");
    assert!(published_objects <= 128 * 20, "{published_objects}");
    Ok(())
}

/// Terminal records that together carry every component-bearing state, each
/// with distinct evidence, so the graph must resolve every reference shape.
fn every_state_operation_records() -> TestResult<Vec<DurableOperationRecord>> {
    let turn_id = id::<TurnId>(7_001)?;
    let attempt_id = id(8_002)?;
    let call_id = id(8_003)?;
    let operation = |label: &[u8]| component(RuntimeComponentKind::Operation, 1, label);
    let at = |checkpoint: u128, state| -> TestResult<DurableOperationTransition> {
        Ok(DurableOperationTransition::new(id(checkpoint)?, state))
    };
    let decision_request = operation(b"decision-request")?;
    Ok(vec![
        DurableOperationRecord::new(
            1,
            DurableOperationKind::Provider {
                root: RuntimeOperationRoot::Turn(turn_id),
                attempt_id,
            },
            operation(b"provider-request")?,
            vec![
                at(8_101, DurableOperationState::Prepared)?,
                at(
                    8_102,
                    DurableOperationState::LocatorBound(operation(b"prepared-locator")?),
                )?,
                at(8_103, DurableOperationState::Dispatched)?,
                at(
                    8_104,
                    DurableOperationState::LocatorBound(operation(b"dispatched-locator")?),
                )?,
                at(
                    8_105,
                    DurableOperationState::ProvenNotDispatched(operation(b"not-dispatched")?),
                )?,
                at(
                    8_106,
                    DurableOperationState::Finalized(operation(b"not-dispatched")?),
                )?,
            ],
        )?,
        DurableOperationRecord::new(
            2,
            DurableOperationKind::Effect {
                turn_id,
                attempt_id,
                call_id,
                source_index: 0,
            },
            operation(b"effect-request")?,
            vec![
                at(8_201, DurableOperationState::Prepared)?,
                at(
                    8_202,
                    DurableOperationState::Claimed(operation(b"effect-claim")?),
                )?,
                at(
                    8_203,
                    DurableOperationState::OutcomeUnknown(operation(b"effect-unknown")?),
                )?,
                at(
                    8_204,
                    DurableOperationState::Completed(operation(b"effect-outcome")?),
                )?,
                at(
                    8_205,
                    DurableOperationState::Finalized(operation(b"effect-outcome")?),
                )?,
            ],
        )?,
        DurableOperationRecord::new(
            3,
            DurableOperationKind::EffectDecision {
                turn_id,
                attempt_id,
                call_id,
            },
            decision_request.clone(),
            vec![
                at(8_301, DurableOperationState::Prepared)?,
                at(
                    8_302,
                    DurableOperationState::DecisionRequired(PendingEffectDecisionState::new(
                        decision_request,
                    )?),
                )?,
                at(
                    8_303,
                    DurableOperationState::Completed(operation(b"decision-outcome")?),
                )?,
                at(
                    8_304,
                    DurableOperationState::Finalized(operation(b"decision-outcome")?),
                )?,
            ],
        )?,
        DurableOperationRecord::new(
            4,
            DurableOperationKind::ToolBatchContinuation {
                turn_id,
                attempt_id,
            },
            operation(b"continuation-request")?,
            vec![
                at(
                    8_401,
                    DurableOperationState::ContinuationRequired(operation(b"continuation-frame")?),
                )?,
                at(
                    8_402,
                    DurableOperationState::Finalized(operation(b"continuation-closed")?),
                )?,
            ],
        )?,
    ])
}

const EVERY_STATE_EVIDENCE: [&[u8]; 13] = [
    b"provider-request",
    b"prepared-locator",
    b"dispatched-locator",
    b"not-dispatched",
    b"effect-request",
    b"effect-claim",
    b"effect-unknown",
    b"effect-outcome",
    b"decision-request",
    b"decision-outcome",
    b"continuation-request",
    b"continuation-frame",
    b"continuation-closed",
];

fn every_state_checkpoint() -> TestResult<RuntimeCheckpoint> {
    fixture_with_history_and_ledger(
        CanonicalHistory::new(vec![history_segment(0)?])?,
        81,
        false,
        Some(RuntimeOperationLedger::new(
            Vec::new(),
            every_state_operation_records()?,
        )?),
        None,
    )
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[test]
fn operation_records_reference_every_state_component_and_reopen_exactly() -> TestResult {
    let checkpoint = every_state_checkpoint()?;
    let bundle = RuntimeCheckpointCodec::bundle(&checkpoint, None)?;
    let records = bundle
        .objects()
        .iter()
        .filter(|object| object.address().kind() == RuntimeObjectKind::OperationRecord)
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 4);
    for record in &records {
        assert!(
            record
                .children()
                .iter()
                .all(|child| child.kind() == RuntimeObjectKind::ReconstructiveComponent)
        );
        for evidence in EVERY_STATE_EVIDENCE {
            assert!(
                !contains(record.canonical_bytes(), evidence),
                "record inlines {}",
                String::from_utf8_lossy(evidence)
            );
        }
    }
    for evidence in EVERY_STATE_EVIDENCE {
        let address = component_address(&component(RuntimeComponentKind::Operation, 1, evidence)?)?;
        assert!(
            records
                .iter()
                .any(|record| record.children().contains(&address)),
            "no record references {}",
            String::from_utf8_lossy(evidence)
        );
    }
    assert_eq!(RuntimeCheckpointCodec::materialize(&bundle)?, checkpoint);
    let reopened = CheckpointObjectBundle::from_store(
        bundle.checkpoint_id(),
        bundle.checkpoint_integrity(),
        bundle.root(),
        bundle.objects().to_vec(),
    )?;
    assert_eq!(reopened.validated_checkpoint()?.as_ref(), &checkpoint);
    Ok(())
}

#[test]
fn a_missing_or_tampered_referenced_component_fails_closed() -> TestResult {
    let checkpoint = every_state_checkpoint()?;
    let bundle = RuntimeCheckpointCodec::bundle(&checkpoint, None)?;
    let claim = component_address(&component(
        RuntimeComponentKind::Operation,
        1,
        b"effect-claim",
    )?)?;
    let position = bundle
        .objects()
        .iter()
        .position(|object| object.address() == claim)
        .ok_or("claim component object is absent")?;
    let reopen = |objects: Vec<RuntimeObject>| {
        CheckpointObjectBundle::from_store(
            bundle.checkpoint_id(),
            bundle.checkpoint_integrity(),
            bundle.root(),
            objects,
        )
        .and_then(|reopened| reopened.validated_checkpoint().map(|_| ()))
    };

    let mut objects = bundle.objects().to_vec();
    objects.remove(position);
    assert_eq!(
        reopen(objects.clone()),
        Err(CheckpointBundleError::MissingObject)
    );
    let mut missing = bundle.clone();
    replace_supplied(&mut missing, objects);
    assert_eq!(
        RuntimeCheckpointCodec::materialize(&missing),
        Err(CheckpointBundleError::MissingObject)
    );

    let forged = encode_bundle_component(&component(
        RuntimeComponentKind::Operation,
        1,
        b"forged-claim",
    )?)?;
    let mut objects = bundle.objects().to_vec();
    objects[position].canonical_bytes = forged.clone().into();
    assert_eq!(
        RuntimeObject::from_store(claim, forged, Vec::new()),
        Err(CheckpointBundleError::InvalidAddress)
    );
    let mut tampered = bundle.clone();
    replace_supplied(&mut tampered, objects);
    assert_eq!(
        RuntimeCheckpointCodec::materialize(&tampered),
        Err(CheckpointBundleError::InvalidAddress)
    );

    let record = bundle
        .objects()
        .iter()
        .find(|object| object.children().contains(&claim))
        .ok_or("no record references the claim")?;
    let mut redirected = record.children().to_vec();
    for child in &mut redirected {
        if *child == claim {
            *child = component_address(&component(
                RuntimeComponentKind::Operation,
                1,
                b"effect-unknown",
            )?)?;
        }
    }
    assert_eq!(
        RuntimeObject::from_store(
            record.address(),
            record.canonical_bytes().to_vec(),
            redirected
        ),
        Err(CheckpointBundleError::NonCanonical)
    );
    Ok(())
}

#[test]
fn an_operation_reference_to_non_operation_evidence_fails_closed() -> TestResult {
    let records = every_state_operation_records()?;
    let mut builder = BundleBuilder {
        objects: BTreeMap::new(),
        visited: 0,
    };
    let foreign = builder.component(&component(
        RuntimeComponentKind::Command,
        1,
        b"not-operation-evidence",
    )?)?;
    let mut request = true;
    let mut children = Vec::new();
    let wire = OperationRecordWire::new(&records[1], |component| {
        let address = if std::mem::take(&mut request) {
            foreign
        } else {
            builder.component(component)?
        };
        children.push(address);
        Ok::<_, CheckpointBundleError>(address.digest())
    })?;
    let address = builder.insert(RuntimeObjectKind::OperationRecord, wire.encode()?, children)?;
    let index = ObjectIndex {
        decoded: BTreeMap::new(),
        objects: builder
            .objects
            .iter()
            .map(|(address, object)| (*address, object))
            .collect(),
    };
    let object = index.object(address, RuntimeObjectKind::OperationRecord)?;
    assert_eq!(
        materialize_operation(&index, object),
        Err(CheckpointBundleError::Manifest(
            RuntimeManifestError::ComponentKind
        ))
    );
    Ok(())
}

#[test]
fn operation_record_size_is_independent_of_component_payload_size() -> TestResult {
    let turn_id = id::<TurnId>(7_001)?;
    let effect = |output: Vec<u8>| -> TestResult<DurableOperationRecord> {
        let output = ReconstructiveComponent::new(RuntimeComponentKind::Operation, 1, output)?;
        Ok(DurableOperationRecord::new(
            1,
            DurableOperationKind::Effect {
                turn_id,
                attempt_id: id(8_502)?,
                call_id: id(8_503)?,
                source_index: 0,
            },
            component(RuntimeComponentKind::Operation, 1, b"effect-request")?,
            vec![
                DurableOperationTransition::new(id(8_504)?, DurableOperationState::Prepared),
                DurableOperationTransition::new(
                    id(8_505)?,
                    DurableOperationState::Claimed(component(
                        RuntimeComponentKind::Operation,
                        1,
                        b"effect-claim",
                    )?),
                ),
                DurableOperationTransition::new(id(8_506)?, DurableOperationState::Dispatched),
                DurableOperationTransition::new(
                    id(8_507)?,
                    DurableOperationState::Completed(output.clone()),
                ),
                DurableOperationTransition::new(
                    id(8_508)?,
                    DurableOperationState::Finalized(output),
                ),
            ],
        )?)
    };
    let small = effect(vec![0x5a; 16])?;
    let payload_bytes = 4 * 1024 * 1024 + 1;
    let large = effect(vec![0x5a; payload_bytes])?;
    let small_bytes = encode_bundle_operation_record(&small)?;
    let large_bytes = encode_bundle_operation_record(&large)?;
    assert_eq!(small_bytes.len(), large_bytes.len());
    assert!(large_bytes.len() < 1024, "{}", large_bytes.len());

    let bundle = RuntimeCheckpointCodec::bundle(
        &fixture_with_history_and_ledger(
            CanonicalHistory::new(Vec::new())?,
            85,
            false,
            Some(RuntimeOperationLedger::new(Vec::new(), vec![large])?),
            None,
        )?,
        None,
    )?;
    let record = bundle
        .objects()
        .iter()
        .find(|object| object.address().kind() == RuntimeObjectKind::OperationRecord)
        .ok_or("no operation record object")?;
    assert_eq!(record.canonical_bytes(), large_bytes.as_slice());
    // The completed and finalized output is one component object, stored once.
    assert_eq!(
        bundle
            .objects()
            .iter()
            .filter(|object| object.canonical_bytes().len() >= payload_bytes)
            .count(),
        1
    );
    assert!(bundle.stats().retained_bytes() < payload_bytes + 64 * 1024);
    let reopened = CheckpointObjectBundle::from_store(
        bundle.checkpoint_id(),
        bundle.checkpoint_integrity(),
        bundle.root(),
        bundle.current_root_closure()?.objects().to_vec(),
    )?;
    assert_eq!(
        reopened.validated_checkpoint()?,
        bundle.validated_checkpoint()?
    );
    Ok(())
}

#[test]
fn advancing_an_operation_with_a_large_request_retains_only_references() -> TestResult {
    let turn_id = id::<TurnId>(7_001)?;
    let history = CanonicalHistory::new(vec![history_segment(0)?])?;
    let request = component(
        RuntimeComponentKind::Operation,
        1,
        &vec![0x6b; 16 * 1024 * 1024 + 1],
    )?;
    let output = component(RuntimeComponentKind::Operation, 1, b"provider-output")?;
    let mut ledger = RuntimeOperationLedger::new(Vec::new(), Vec::new())?.append_operation(
        DurableOperationRecord::new(
            1,
            DurableOperationKind::Provider {
                root: RuntimeOperationRoot::Turn(turn_id),
                attempt_id: id(8_602)?,
            },
            request,
            vec![DurableOperationTransition::new(
                id(8_603)?,
                DurableOperationState::Prepared,
            )],
        )?,
    )?;
    let mut bundle = RuntimeCheckpointCodec::bundle(
        &fixture_with_history_and_ledger(history.clone(), 1, true, Some(ledger.clone()), None)?,
        None,
    )?;
    let mut seen = bundle
        .objects()
        .iter()
        .map(|object| (object.address(), object.clone()))
        .collect::<BTreeMap<_, _>>();
    // Prepared -> Dispatched -> LocatorBound while working, then the
    // completed outcome is finalized into a quiescent checkpoint.
    let steps = [
        (vec![(8_604, DurableOperationState::Dispatched)], true),
        (
            vec![(
                8_605,
                DurableOperationState::LocatorBound(component(
                    RuntimeComponentKind::Operation,
                    1,
                    b"provider-locator",
                )?),
            )],
            true,
        ),
        (
            vec![
                (8_606, DurableOperationState::Completed(output.clone())),
                (8_607, DurableOperationState::Finalized(output)),
            ],
            false,
        ),
    ];
    for (number, (transitions, working)) in (2_u128..).zip(steps) {
        let transitions = transitions
            .into_iter()
            .map(|(checkpoint, state)| Ok(DurableOperationTransition::new(id(checkpoint)?, state)))
            .collect::<TestResult<Vec<_>>>()?;
        ledger = ledger.transition_operation_many(1, transitions)?;
        let checkpoint = fixture_with_history_and_ledger(
            history.clone(),
            number,
            working,
            Some(ledger.clone()),
            None,
        )?;
        bundle = RuntimeCheckpointCodec::advance(&checkpoint, &bundle)?;
        assert!(
            bundle.stats().new_retained_bytes() < 16 * 1024,
            "step {number} retained {} new bytes",
            bundle.stats().new_retained_bytes()
        );
        assert_eq!(RuntimeCheckpointCodec::materialize(&bundle)?, checkpoint);
        seen.extend(
            bundle
                .objects()
                .iter()
                .map(|object| (object.address(), object.clone())),
        );
        let mut closure = BTreeMap::new();
        let mut pending = vec![bundle.root()];
        while let Some(address) = pending.pop() {
            if closure.contains_key(&address) {
                continue;
            }
            let object = seen.get(&address).ok_or("closure object is absent")?;
            pending.extend(object.children().iter().copied());
            closure.insert(address, object.clone());
        }
        let reopened = CheckpointObjectBundle::from_store(
            bundle.checkpoint_id(),
            bundle.checkpoint_integrity(),
            bundle.root(),
            closure.into_values().collect(),
        )?;
        assert_eq!(reopened.validated_checkpoint()?.as_ref(), &checkpoint);
    }
    Ok(())
}

fn multi_operation_fixture() -> TestResult<(
    CanonicalHistory,
    ReconstructiveComponent,
    RuntimeOperationLedger,
    RuntimeCheckpoint,
)> {
    let history = CanonicalHistory::new(vec![history_segment(0)?])?;
    let evidence = component(RuntimeComponentKind::Operation, 1, b"landed")?;
    let records = (1..=5)
        .map(|key| {
            Ok(DurableOperationRecord::new(
                key,
                DurableOperationKind::Provider {
                    root: RuntimeOperationRoot::Turn(id(7_001)?),
                    attempt_id: id(8_000 + u128::from(key))?,
                },
                evidence.clone(),
                vec![
                    DurableOperationTransition::new(id(8_101)?, DurableOperationState::Prepared),
                    DurableOperationTransition::new(id(8_102)?, DurableOperationState::Dispatched),
                    DurableOperationTransition::new(
                        id(8_103)?,
                        DurableOperationState::Completed(evidence.clone()),
                    ),
                ],
            )?)
        })
        .collect::<TestResult<Vec<_>>>()?;
    let ledger = RuntimeOperationLedger::new(Vec::new(), records)?;
    let lifecycle = RuntimeLifecycle::Working {
        root: RuntimeWorkingRoot::Turn {
            turn_id: id(7_001)?,
            next_round: 1,
        },
        recovery_phase: WorkingRecoveryPhase::Finalization,
        obligations: (1..=5)
            .map(|key| {
                RecoveryObligation::new(
                    key,
                    RecoveryHandlerSignature::new(
                        RecoveryObligationKind::ProviderAttempt,
                        RecoveryAction::FinalizationRequired,
                    ),
                )
            })
            .collect(),
    };
    let source = fixture_with_history_and_ledger(
        history.clone(),
        1,
        true,
        Some(ledger.clone()),
        Some(lifecycle),
    )?;
    Ok((history, evidence, ledger, source))
}

#[test]
fn multi_operation_finalization_supplies_a_closed_dag_after_cold_load() -> TestResult {
    let (history, evidence, mut ledger, source) = multi_operation_fixture()?;
    let warm = RuntimeCheckpointCodec::bundle(&source, None)?;
    let cold = CheckpointObjectBundle::from_store(
        warm.checkpoint_id(),
        warm.checkpoint_integrity(),
        warm.root(),
        warm.objects().to_vec(),
    )?;
    for key in 1..=5 {
        ledger = ledger.transition_operation(
            key,
            DurableOperationTransition::new(
                id(2)?,
                DurableOperationState::Finalized(evidence.clone()),
            ),
        )?;
    }
    let terminal = fixture_with_history_and_ledger(history, 2, false, Some(ledger), None)?;
    for prior in [&warm, &cold] {
        let advanced = RuntimeCheckpointCodec::advance(&terminal, prior)?;
        let supplied = advanced
            .supplied_objects()
            .iter()
            .map(RuntimeObject::address)
            .collect::<BTreeSet<_>>();
        for object in advanced.supplied_objects() {
            for child in object.children() {
                assert!(
                    supplied.contains(child) || prior.lookup(*child).is_some(),
                    "supplied {:?} has missing child {:?}",
                    object.address(),
                    child
                );
            }
        }
        assert_eq!(RuntimeCheckpointCodec::materialize(&advanced)?, terminal);
    }
    Ok(())
}

#[test]
fn live_multi_operation_advance_preserves_root_and_forgets_pruned_objects() -> TestResult {
    let (history, evidence, mut ledger, source) = multi_operation_fixture()?;
    let prior = RuntimeCheckpointCodec::bundle(&source, None)?;
    for key in 1..=2 {
        ledger = ledger.transition_operation(
            key,
            DurableOperationTransition::new(
                id(2)?,
                DurableOperationState::Finalized(evidence.clone()),
            ),
        )?;
    }
    let mut lifecycle = source.manifest().lifecycle().clone();
    let RuntimeLifecycle::Working { obligations, .. } = &mut lifecycle else {
        return Err("fixture is not working".into());
    };
    obligations.retain(|obligation| obligation.operation_id() > 2);
    let live = fixture_with_history_and_ledger(
        history.clone(),
        2,
        true,
        Some(ledger.clone()),
        Some(lifecycle),
    )?;
    // This is the original construction, before candidate pruning.
    let mut raw = BundleBuilder {
        objects: BTreeMap::new(),
        visited: 0,
    };
    let built = raw.advance(
        &live,
        &source,
        &prior,
        prior.reuse.as_deref().ok_or("reuse missing")?,
    )?;
    let advanced = RuntimeCheckpointCodec::advance(&live, &prior)?;
    assert_eq!(built.root, advanced.root());
    for object in advanced.supplied_objects() {
        assert_eq!(raw.objects.get(&object.address()), Some(object));
    }
    let pruned = raw
        .objects
        .keys()
        .copied()
        .filter(|address| prior.lookup(*address).is_none() && advanced.lookup(*address).is_none())
        .collect::<BTreeSet<_>>();
    assert!(
        pruned
            .iter()
            .any(|address| address.kind() == RuntimeObjectKind::OperationNode)
    );
    assert_eq!(
        advanced.layers.objects.as_ref(),
        advanced.supplied_objects()
    );
    assert_eq!(
        advanced.stats().new_object_count(),
        advanced.supplied_objects().len()
    );
    let mut layer = Some(advanced.layers.as_ref());
    while let Some(current) = layer {
        assert!(
            current
                .objects
                .iter()
                .all(|object| !pruned.contains(&object.address()))
        );
        layer = current.prior.as_deref();
    }
    assert_eq!(RuntimeCheckpointCodec::materialize(&advanced)?, live);
    for key in 3..=5 {
        ledger = ledger.transition_operation(
            key,
            DurableOperationTransition::new(
                id(3)?,
                DurableOperationState::Finalized(evidence.clone()),
            ),
        )?;
    }
    let terminal = fixture_with_history_and_ledger(history, 3, false, Some(ledger), None)?;
    let successor = RuntimeCheckpointCodec::advance(&terminal, &advanced)?;
    assert!(
        pruned
            .iter()
            .all(|address| successor.lookup(*address).is_none())
    );
    let supplied = successor
        .supplied_objects()
        .iter()
        .map(RuntimeObject::address)
        .collect::<BTreeSet<_>>();
    assert!(successor.supplied_objects().iter().all(|object| {
        object
            .children()
            .iter()
            .all(|child| supplied.contains(child) || advanced.lookup(*child).is_some())
    }));
    assert_eq!(RuntimeCheckpointCodec::materialize(&successor)?, terminal);
    Ok(())
}

#[test]
fn foreign_or_noncanonical_operation_record_bytes_fail_closed() -> TestResult {
    // The effect-decision record: id, kind tag 3 and three ids, the request
    // reference, the count, then Prepared and DecisionRequired transitions first.
    const REQUEST: usize = 8 + 1 + 48;
    const FIRST_TAG: usize = REQUEST + 38 + 8 + 16;
    const SECOND_TAG: usize = FIRST_TAG + 1 + 16;
    const MINTED: usize = SECOND_TAG + 1 + 38;
    let record = &every_state_operation_records()?[2];
    let valid = encode_bundle_operation_record(record)?;
    assert_eq!(valid[REQUEST], 1);
    assert_eq!(valid[REQUEST + 1], 5);
    assert_eq!(
        valid[REQUEST + 2..REQUEST + 6],
        RUNTIME_FORMAT_EPOCH.to_be_bytes()
    );
    assert_eq!(valid[FIRST_TAG], 1);
    assert_eq!(valid[SECOND_TAG], 9);
    assert_eq!(valid[MINTED], 0);

    let mutate = |change: &dyn Fn(&mut Vec<u8>)| {
        let mut bytes = valid.clone();
        change(&mut bytes);
        bytes
    };
    let cases: [(&str, Vec<u8>, CheckpointBundleError); 6] = [
        (
            "foreign digest domain",
            mutate(&|bytes| bytes[REQUEST + 1] = 16),
            CheckpointBundleError::Manifest(RuntimeManifestError::InvalidDigest),
        ),
        (
            "foreign digest version",
            mutate(&|bytes| {
                bytes[REQUEST + 2..REQUEST + 6]
                    .copy_from_slice(&(RUNTIME_FORMAT_EPOCH - 1).to_be_bytes());
            }),
            CheckpointBundleError::UnsupportedRevision,
        ),
        (
            "minted dispatch capability",
            mutate(&|bytes| bytes[MINTED] = 1),
            CheckpointBundleError::Manifest(RuntimeManifestError::InvalidLedger),
        ),
        (
            "noncanonical optional flag",
            mutate(&|bytes| bytes[MINTED] = 2),
            CheckpointBundleError::Manifest(RuntimeManifestError::UnknownTag {
                field: "optional",
                tag: 2,
            }),
        ),
        (
            "unknown state tag",
            mutate(&|bytes| bytes[FIRST_TAG] = 11),
            CheckpointBundleError::Manifest(RuntimeManifestError::UnknownTag {
                field: "operation state",
                tag: 11,
            }),
        ),
        (
            "trailing bytes",
            mutate(&|bytes| bytes.push(0)),
            CheckpointBundleError::Manifest(RuntimeManifestError::TrailingBytes),
        ),
    ];
    for (name, bytes, expected) in cases {
        let address = object_address(RuntimeObjectKind::OperationRecord, &bytes)?;
        assert_eq!(
            RuntimeObject::from_store(address, bytes, Vec::new()),
            Err(expected),
            "{name}"
        );
    }
    Ok(())
}

#[test]
fn cold_load_hashes_each_object_and_decodes_each_segment_once() -> TestResult {
    let checkpoint = fixture(8, 9001, false)?;
    let bundle = RuntimeCheckpointCodec::bundle(&checkpoint, None)?;
    let segment_count = bundle
        .supplied_objects()
        .iter()
        .filter(|object| object.address().kind() == RuntimeObjectKind::HistorySegment)
        .count();
    SEGMENT_DECODES.set(0);
    SEGMENT_HASHES.set(0);
    ADDRESS_VALIDATIONS.set(0);
    let objects = bundle
        .supplied_objects()
        .iter()
        .map(|object| {
            RuntimeObject::from_store(
                object.address(),
                object.canonical_bytes().to_vec(),
                object.children().to_vec(),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(ADDRESS_VALIDATIONS.get(), bundle.supplied_objects().len());
    let loaded = CheckpointObjectBundle::from_store(
        bundle.checkpoint_id(),
        bundle.checkpoint_integrity(),
        bundle.root(),
        objects,
    )?;
    assert_eq!(SEGMENT_DECODES.get(), segment_count);
    assert_eq!(SEGMENT_HASHES.get(), segment_count);
    assert_eq!(ADDRESS_VALIDATIONS.get(), bundle.supplied_objects().len());
    let materialized = loaded.validated_checkpoint()?;
    assert_eq!(*materialized, checkpoint);
    Ok(())
}

#[test]
fn trusted_builder_validates_only_new_history_segment_payloads() -> TestResult {
    let checkpoint = fixture(64, 1, false)?;
    let history = checkpoint
        .manifest()
        .canonical()
        .history()
        .append(history_segment(64)?)?;
    let prior = RuntimeCheckpointCodec::bundle(&checkpoint, None)?;
    let next = fixture_with_history(history, 2, false)?;
    SEGMENT_DECODES.set(0);
    let delta = RuntimeCheckpointCodec::advance(&next, &prior)?;
    assert_eq!(
        delta
            .supplied_objects()
            .iter()
            .filter(|object| object.address().kind() == RuntimeObjectKind::HistorySegment)
            .count(),
        1
    );
    assert_eq!(SEGMENT_DECODES.get(), 1);
    let mut builder = BundleBuilder {
        objects: BTreeMap::new(),
        visited: 0,
    };
    assert!(
        builder
            .insert(
                RuntimeObjectKind::HistorySegment,
                b"unvalidated".to_vec(),
                Vec::new()
            )
            .is_err()
    );
    Ok(())
}

#[test]
fn trusted_publication_refuses_a_lossy_segment_roundtrip() -> TestResult {
    let citation = plakous_model::CitationBlock::new(
        plakous_model::CitationTarget::new(id(1)?, 0),
        plakous_model::CitationSource::new("source", "annotation", None, None)?,
        Some(plakous_model::CitationRange::new(
            0,
            9_007_199_254_740_993,
            plakous_model::CitationUnit::Utf8Byte,
        )?),
    );
    let segment = HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![ConversationItem::new(
            id(1)?,
            Actor::Assistant,
            vec![ContentBlock::Citation(citation)],
            Provenance::ModelOutput,
        )?],
    )?;
    let mut builder = BundleBuilder {
        objects: BTreeMap::new(),
        visited: 0,
    };
    assert!(matches!(
        builder.segment(&segment),
        Err(CheckpointBundleError::NonCanonical)
    ));
    Ok(())
}

#[test]
fn builder_just_addressed_segment_is_roundtripped_without_rehashing() -> TestResult {
    let segment = history_segment(0)?;
    let mut builder = BundleBuilder {
        objects: BTreeMap::new(),
        visited: 0,
    };
    OBJECT_HASHES.set(0);
    ADDRESS_VALIDATIONS.set(0);
    SEGMENT_DECODES.set(0);
    builder.segment(&segment)?;
    assert_eq!(OBJECT_HASHES.get(), 1);
    assert_eq!(ADDRESS_VALIDATIONS.get(), 0);
    assert_eq!(SEGMENT_DECODES.get(), 1);
    Ok(())
}

#[test]
fn history_and_working_materializers_share_the_decoded_segment() -> TestResult {
    let checkpoint = fixture(1, 9200, false)?;
    let bundle = RuntimeCheckpointCodec::bundle(&checkpoint, None)?;
    let segment = checkpoint
        .manifest()
        .canonical()
        .history()
        .segments()
        .iter()
        .next()
        .ok_or("segment")?
        .clone();
    let working = WorkingProjection::new(vec![segment], WorkingProjection::default().limits())?;
    let mut builder = BundleBuilder {
        objects: BTreeMap::new(),
        visited: 0,
    };
    let working_address = builder.working(&working)?;
    SEGMENT_DECODES.set(0);
    let mut index = ObjectIndex::validate(&bundle)?;
    let decoded = SEGMENT_DECODES.get();
    index.objects.insert(
        working_address,
        builder
            .objects
            .get(&working_address)
            .ok_or("working object")?,
    );
    let history = materialize_history(
        &index,
        1,
        bundle.reuse.as_ref().ok_or("reuse")?.history_head,
    )?;
    let working = materialize_working(&index, working_address)?;
    assert!(
        history
            .segments()
            .iter()
            .next()
            .ok_or("history segment")?
            .shares_storage_with(&working.segments()[0])
    );
    assert_eq!(SEGMENT_DECODES.get(), decoded);
    Ok(())
}
