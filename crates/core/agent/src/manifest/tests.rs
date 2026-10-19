use super::codec::{
    obligation_kind, obligation_kind_tag, recovery_action, recovery_action_tag, recovery_phase,
    recovery_phase_tag,
};
use super::context_state::{
    compaction_provenance_identity, read_compaction_provenance, write_compaction_provenance_fields,
};
use super::*;
use crate::checkpoint_bundle::RUNTIME_FORMAT_EPOCH;
use crate::{
    CompilerModelFacts, CompilerRuntimeFacts, CompilerToolFacts, FeatureSupportFact,
    HARNESS_PLAN_COMPONENT_FORMAT_VERSION, HarnessCompilerInputs, HarnessPlanComponentEnvelope,
    HarnessProfileCompiler, HarnessProfileId, HarnessProfileSpecBuilder, RuntimeRehydration,
    RuntimeRevalidationWitness,
};
use plakous_model::{
    Actor, CachePolicy, CapabilitySet, CompactionBlock, ContentBlock, ContextAdmissionRequirement,
    ConversationItem, CredentialSelectionId, EffectiveProviderConfiguration, EntryId,
    FamilyPolicyEnvelope, GenerationLimits, HistorySegmentKind, LocalDurabilityDisposition,
    ModelAdapterProfileId, ModelId, ModelImplementationIdentity, NativeOptions,
    OutputBoundDisposition, OutputPolicy, ParallelToolDisposition, Provenance,
    ProviderAuthorityClassId, ProviderBehaviorConfiguration, ProviderProfileRevisionId,
    ProviderRouteSelection, ProviderSemanticEvidence, ProviderStorageDisposition,
    ReasoningEffortDisposition, ReasoningIncludeDisposition, ReasoningPolicy,
    ReasoningSummaryDisposition, ReplayDisposition, ResolvedModelDescriptor,
    ResolvedProviderAuthorityBinding, ResponseStateDisposition, RetentionDisposition,
    RuntimeModelAuthority, SelectedProviderConfiguration, TextBlock, ToolCallState, ToolChoice,
    ToolCorrelation, ToolExecutionOwner, ToolName,
};
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn numeric_tool_segment(raw: &str) -> TestResult<HistorySegment> {
    let call_id = CallId::try_from(Uuid::from_u128(101))?;
    let correlation = ToolCorrelation::default();
    Ok(HistorySegment::new(
        HistorySegmentKind::ToolExchange,
        vec![
            ConversationItem::new(
                EntryId::try_from(Uuid::from_u128(102))?,
                Actor::Assistant,
                vec![ContentBlock::ToolCall(ToolCallBlock::new(
                    call_id,
                    ToolName::new("sum")?,
                    ToolExecutionOwner::Local,
                    ToolCallState::Requested,
                    correlation.clone(),
                    raw,
                )?)],
                Provenance::ModelOutput,
            )?,
            ConversationItem::new(
                EntryId::try_from(Uuid::from_u128(103))?,
                Actor::Tool,
                vec![ContentBlock::ToolResult(
                    plakous_model::ToolResultBlock::new(
                        call_id,
                        ToolExecutionOwner::Local,
                        plakous_model::ToolResultKind::Success,
                        vec![plakous_model::ToolResultPart::Text(TextBlock::new(
                            "ok", None,
                        )?)],
                        correlation,
                    )?,
                )],
                Provenance::ModelOutput,
            )?,
        ],
    )?)
}

#[test]
fn numeric_tool_arguments_round_trip_checkpoint_segment() -> TestResult {
    for raw in [
        "null",
        r#"{"a":21345.0,"b":42171.0}"#,
        r#"{"nested":[-0.0,1e2,1.5,9007199254740993,18446744073709551615]}"#,
    ] {
        let segment = numeric_tool_segment(raw)?;
        let mut writer = Writer::default();
        writer.segment(&segment)?;
        let decoded = Reader::new(&writer.bytes).segment()?;
        assert_eq!(decoded, segment);
        assert_eq!(decoded.canonical_bytes(), segment.canonical_bytes());
        assert_eq!(decoded.identity(), segment.identity());
    }
    Ok(())
}

#[test]
fn numeric_tool_segment_persisted_bytes_and_digest_are_unchanged() -> TestResult {
    let segment = numeric_tool_segment(r#"{"a":21345.0,"b":42171.0}"#)?;
    let canonical = include_bytes!("../../tests/fixtures/numeric_tool_segment.json");
    let digest = [
        251, 101, 55, 24, 104, 72, 142, 37, 213, 140, 120, 119, 170, 86, 143, 76, 6, 118, 254, 60,
        18, 172, 88, 226, 200, 29, 191, 18, 152, 96, 13, 192,
    ];
    assert_eq!(segment.canonical_bytes(), canonical);
    assert_eq!(segment.identity().bytes(), &digest);

    // Pin the checkpoint segment framing as well as the model-owned bytes.
    let mut expected = u32::try_from(canonical.len())?.to_be_bytes().to_vec();
    expected.extend_from_slice(canonical);
    expected.extend_from_slice(&[1, 16, 0, 0, 0, 2]);
    expected.extend_from_slice(&digest);
    let mut writer = Writer::default();
    writer.segment(&segment)?;
    assert_eq!(writer.bytes, expected);
    Ok(())
}

/// Encodes `record`, then decodes it resolving each referenced component from
/// the record's own components, as the checkpoint graph resolves them from
/// its component objects.
fn assert_operation_round_trips(record: &DurableOperationRecord) -> TestResult {
    let mut components = BTreeMap::new();
    OperationRecordWire::new(record, |component| {
        let digest = crate::checkpoint_bundle::component_object_digest(component)?;
        components.insert(*digest.bytes(), component.clone());
        Ok::<_, RuntimeManifestError>(digest)
    })?;
    let decoded = OperationRecordWire::decode(&encode_bundle_operation_record(record)?)?.resolve(
        |digest| {
            components
                .get(digest.bytes())
                .cloned()
                .ok_or(RuntimeManifestError::Malformed)
        },
    )?;
    assert_eq!(&decoded, record);
    Ok(())
}

const PROVIDER_HANDLER: RecoveryHandlerSignature = RecoveryHandlerSignature::new(
    RecoveryObligationKind::ProviderAttempt,
    RecoveryAction::ProviderEvidenceRequired,
);

fn digest(domain: DigestDomain, label: &'static [u8]) -> TestResult<Digest> {
    Ok(Digest::sha256(domain, 1, &[label])?)
}

fn route_binding_fixture(
    route: &'static str,
    account_reference: &'static str,
    admission_label: &'static [u8],
) -> TestResult<ModelRouteBinding> {
    let account = BindingReference::new("account-authority", account_reference, 1)?;
    Ok(ModelRouteBinding::new(
        ProtocolId::new("test.protocol")?,
        RouteId::new(route)?,
        ModelId::new("test-model")?,
        None,
        None,
        ModelAdapterProfileId::new("test.adapter")?,
        account.clone(),
        BindingReference::new("profile-authority", "profile-ref", 1)?,
        digest(DigestDomain::ModelBinding, b"route-binding-model")?,
        component(
            RuntimeComponentKind::ModelDescriptor,
            b"route-binding-descriptor",
        )?,
        RevalidationFact::new(
            account,
            Timestamp::new(10, 0)?,
            Some(Timestamp::new(20, 0)?),
            component(RuntimeComponentKind::AdmissionEvidence, admission_label)?,
        )?,
    )?)
}

fn component(
    kind: RuntimeComponentKind,
    label: &'static [u8],
) -> TestResult<ReconstructiveComponent> {
    let revision = match kind {
        RuntimeComponentKind::ToolRegistry => TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION,
        RuntimeComponentKind::ToolExposure => TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION,
        _ => 1,
    };
    Ok(ReconstructiveComponent::new(kind, revision, label)?)
}

/// Abandoning an unfinished model request is the only way one closes unproven.
///
/// The transition table is the whole of the policy. `OutcomeUnknown` used to be
/// a dead end, which is why a session whose process died mid-turn could never
/// be opened again: nothing could move its provider attempt to a terminal
/// state, so the turn stayed working for ever. It now finalizes, carrying the
/// same evidence it recorded as unknown, and the two have to agree, because
/// there is only one thing to say about an attempt nobody observed.
#[test]
fn an_unknown_outcome_finalizes_only_with_the_evidence_it_recorded() -> TestResult {
    let evidence = component(RuntimeComponentKind::Operation, b"abandoned-v1")?;
    let contradiction = component(RuntimeComponentKind::Operation, b"something-else")?;
    let ledger = RuntimeOperationLedger::new(
        Vec::new(),
        vec![DurableOperationRecord::new(
            7,
            DurableOperationKind::Provider {
                root: RuntimeOperationRoot::Turn(id(20)?),
                attempt_id: id(21)?,
            },
            component(RuntimeComponentKind::Operation, b"provider-request-v1")?,
            vec![
                DurableOperationTransition::new(id(22)?, DurableOperationState::Prepared),
                DurableOperationTransition::new(id(23)?, DurableOperationState::Dispatched),
            ],
        )?],
    )?;

    let unknown = ledger.transition_operation(
        7,
        DurableOperationTransition::new(
            id(60_001)?,
            DurableOperationState::OutcomeUnknown(evidence.clone()),
        ),
    )?;
    assert!(
        unknown
            .transition_operation(
                7,
                DurableOperationTransition::new(
                    id(60_001)?,
                    DurableOperationState::Finalized(evidence.clone()),
                ),
            )
            .is_ok(),
        "an abandoned attempt closes carrying what it recorded"
    );
    assert_eq!(
        unknown.transition_operation(
            7,
            DurableOperationTransition::new(
                id(60_001)?,
                DurableOperationState::Finalized(contradiction),
            ),
        ),
        Err(RuntimeManifestError::InvalidLedger),
        "two different stories about one attempt is not one abandonment"
    );
    assert_eq!(
        ledger.transition_operation(
            7,
            DurableOperationTransition::new(
                id(60_002)?,
                DurableOperationState::Finalized(evidence),
            ),
        ),
        Err(RuntimeManifestError::InvalidLedger),
        "what is unknown has to be recorded before it is closed"
    );
    Ok(())
}

#[test]
fn route_binding_identity_covers_route_account_and_admission() -> TestResult {
    let baseline = route_binding_fixture("test.route", "account-a", b"admission-a")?
        .route_binding_identity()?;
    for changed in [
        route_binding_fixture("test.other-route", "account-a", b"admission-a")?,
        route_binding_fixture("test.route", "account-b", b"admission-a")?,
        route_binding_fixture("test.route", "account-a", b"admission-b")?,
    ] {
        assert_ne!(changed.route_binding_identity()?, baseline);
        assert_eq!(
            changed.model_binding(),
            digest(DigestDomain::ModelBinding, b"route-binding-model")?
        );
    }
    Ok(())
}

fn provider_configuration() -> TestResult<CompiledProviderConfiguration> {
    provider_configuration_with_bound(OutputBoundDisposition::NoRequestBound)
}

fn provider_configuration_with_bound(
    output_bound: OutputBoundDisposition,
) -> TestResult<CompiledProviderConfiguration> {
    let descriptor = ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("test.protocol")?,
        RouteId::new("test.route")?,
        ModelId::new("test-model")?,
        Some(ModelId::new("test-model-reported")?),
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
        output_bound,
        storage: ProviderStorageDisposition::OmittedRouteContract,
        retention: RetentionDisposition::RouteContract {
            profile: descriptor.profile().clone(),
            revision: ProviderProfileRevisionId::new("manifest-retention-v1")?,
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
            ProviderProfileRevisionId::new("manifest-integration-v1")?,
            ModelAdapterProfileId::new("manifest.auth")?,
            ProviderProfileRevisionId::new("manifest-auth-v1")?,
            ProviderAuthorityClassId::new("manifest-test")?,
            CredentialSelectionId::new("manifest-test-credential")?,
            descriptor.requested_model().clone(),
        ),
        behavior.clone(),
    )?;
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        digest(DigestDomain::ModelBinding, b"manifest-account")?,
        digest(DigestDomain::ModelBinding, b"manifest-credential-authority")?,
        1,
        Timestamp::new(10, 0)?,
        Some(Timestamp::new(20, 0)?),
    )?;
    let stable_authority = ResolvedProviderAuthorityBinding::new(
        &descriptor,
        &authority,
        ProviderSemanticEvidence::new(1, b"manifest-provider-semantics-v1".to_vec())?,
    )?;
    let family_policy = FamilyPolicyEnvelope::new(
        descriptor.model_family().cloned().ok_or("model family")?,
        1,
        1,
        1,
        b"manifest-family-policy-v1".to_vec(),
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

/// A session created under one derived bound reopens under another, and says so.
///
/// A derived bound follows what the route's endpoints attest, so it moves
/// when the provider's catalog moves. Refusing the session would make it
/// unresumable for a reason its operator cannot act on, so reopen re-derives
/// the figure and records the change. Everything else still has to match.
#[test]
fn a_reopen_re_derives_a_moved_route_bound_and_records_both_figures() -> TestResult {
    let previous = std::num::NonZeroU32::new(4_096).ok_or("previous")?;
    let current = std::num::NonZeroU32::new(32_768).ok_or("current")?;
    let manifest = manifest_with_bounds(
        ProducerProvenance::new("plakous", "rederive-test", "offline")?,
        None,
        Some((
            OutputBoundDisposition::DerivedFromRoute(previous),
            OutputBoundDisposition::DerivedFromRoute(current),
        )),
    )?;
    let rederived = manifest
        .output_bound_rederived()
        .ok_or("the reopen recorded no re-derivation")?;
    assert_eq!(rederived.previous(), Some(previous));
    assert_eq!(rederived.current(), Some(current));
    Ok(())
}

/// A session created before the route derived its bound still reopens.
///
/// Every session on a route that now derives its bound persisted that bound as
/// `Enforced`, because the derived disposition did not exist when they were
/// written. They are the same sessions on the same routes, so both spellings of
/// the persisted bound are admitted once the live behavior is derived.
#[test]
fn a_session_persisted_before_the_marker_existed_still_reopens() -> TestResult {
    let previous = std::num::NonZeroU32::new(4_096).ok_or("previous")?;
    let current = std::num::NonZeroU32::new(32_768).ok_or("current")?;
    let manifest = manifest_with_bounds(
        ProducerProvenance::new("plakous", "rederive-test", "offline")?,
        None,
        Some((
            // What every existing OpenRouter session carries.
            OutputBoundDisposition::Enforced(previous),
            OutputBoundDisposition::DerivedFromRoute(current),
        )),
    )?;
    let rederived = manifest
        .output_bound_rederived()
        .ok_or("the reopen recorded no re-derivation")?;
    assert_eq!(rederived.previous(), Some(previous));
    assert_eq!(rederived.current(), Some(current));
    Ok(())
}

/// A bound the product chose is not allowed to move. Only a derived one is.
#[test]
fn a_reopen_refuses_a_moved_bound_the_product_chose() -> TestResult {
    let previous = std::num::NonZeroU32::new(4_096).ok_or("previous")?;
    let current = std::num::NonZeroU32::new(32_768).ok_or("current")?;
    assert!(
        manifest_with_bounds(
            ProducerProvenance::new("plakous", "rederive-test", "offline")?,
            None,
            Some((
                OutputBoundDisposition::Enforced(previous),
                OutputBoundDisposition::Enforced(current),
            ))
        )
        .is_err(),
        "a constant bound moved without refusal"
    );
    // A derived bound that did not move records nothing.
    let unchanged = manifest_with_bounds(
        ProducerProvenance::new("plakous", "rederive-test", "offline")?,
        None,
        Some((
            OutputBoundDisposition::DerivedFromRoute(current),
            OutputBoundDisposition::DerivedFromRoute(current),
        )),
    )?;
    assert!(unchanged.output_bound_rederived().is_none());
    Ok(())
}

#[test]
fn provider_configuration_codec_revision_4_roundtrips_processing_tier() -> TestResult {
    assert_eq!(RUNTIME_PROVIDER_CONFIGURATION_CODEC_REVISION, 4);
    let configuration = provider_configuration()?;
    let model_implementation = configuration.effective().model_implementation().clone();

    let encoded = encode_bundle_provider_configuration(&configuration)?;
    let decoded = decode_bundle_provider_configuration(&encoded)?;

    assert_eq!(decoded, configuration);
    assert_eq!(
        decoded.effective().model_implementation(),
        &model_implementation
    );
    Ok(())
}

#[test]
fn provider_configuration_codec_rejects_predecessor_revisions() -> TestResult {
    for revision in [2_u32, 3] {
        let mut encoded = encode_bundle_provider_configuration(&provider_configuration()?)?;
        encoded[8..12].copy_from_slice(&revision.to_be_bytes());

        assert_eq!(
            decode_bundle_provider_configuration(&encoded),
            Err(RuntimeManifestError::UnsupportedVersion { found: revision })
        );
    }
    Ok(())
}

fn plan_component(
    configuration: &CompiledProviderConfiguration,
) -> TestResult<(ReconstructiveComponent, Digest)> {
    let spec = HarnessProfileSpecBuilder::new()
        .id(HarnessProfileId::new("runtime.fixture")?)
        .base_instructions("exact neutral runtime fixture")
        .build()?;
    let model = CompilerModelFacts::new(
        configuration.selected_identity(),
        configuration.effective_identity(),
        configuration.effective().descriptor().protocol().clone(),
        configuration.effective().descriptor().route().clone(),
        configuration
            .effective()
            .descriptor()
            .requested_model()
            .clone(),
        configuration.effective().descriptor().profile().clone(),
        configuration
            .effective()
            .descriptor()
            .semantic_binding_digest()?,
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
        crate::MAX_INSTRUCTION_BYTES,
        false,
        false,
        false,
    );
    let inputs = HarnessCompilerInputs::new(model, tools, runtime, Vec::new())?;
    let plan = HarnessProfileCompiler::new().compile(&spec, &inputs)?;
    let plan_identity = *plan.plan_digest();
    let bytes =
        HarnessPlanComponentEnvelope::capture(&spec, &inputs, &plan, RUNTIME_POLICY_REVISION)?
            .encode()?;
    Ok((
        ReconstructiveComponent::new(
            RuntimeComponentKind::HarnessPlan,
            HARNESS_PLAN_COMPONENT_FORMAT_VERSION,
            bytes,
        )?,
        plan_identity,
    ))
}

fn id<T>(value: u128) -> TestResult<T>
where
    T: TryFrom<Uuid>,
    T::Error: std::error::Error + 'static,
{
    Ok(T::try_from(Uuid::from_u128(value))?)
}

fn workflow_id(byte: u8) -> TestResult<SessionWorkflowRootId> {
    Ok(SessionWorkflowRootId::new(
        [byte; plakous_model::SESSION_WORKFLOW_ROOT_ID_BYTES],
    )?)
}

fn manifest(
    provenance: ProducerProvenance,
    lifecycle_and_ledger: Option<(RuntimeLifecycle, RuntimeOperationLedger)>,
) -> TestResult<RuntimeManifest> {
    manifest_with_bounds(provenance, lifecycle_and_ledger, None)
}

/// One reopen, optionally with a persisted bound that differs from the live one.
///
/// `bounds` is `(persisted, live)`: the prefix and the harness plan are built
/// from a configuration carrying `persisted`, exactly as the session that
/// created them would have, and the manifest is handed a configuration carrying
/// `live`, exactly as a fresh derivation would produce. `None` keeps the
/// original fixture.
fn manifest_with_bounds(
    provenance: ProducerProvenance,
    lifecycle_and_ledger: Option<(RuntimeLifecycle, RuntimeOperationLedger)>,
    bounds: Option<(OutputBoundDisposition, OutputBoundDisposition)>,
) -> TestResult<RuntimeManifest> {
    let persisted_configuration = match bounds {
        Some((persisted, _)) => provider_configuration_with_bound(persisted)?,
        None => provider_configuration()?,
    };
    let provider_configuration = match bounds {
        Some((_, live)) => provider_configuration_with_bound(live)?,
        None => provider_configuration()?,
    };
    let descriptor = provider_configuration.effective().descriptor();
    let (harness_plan, plan_identity) = plan_component(&persisted_configuration)?;
    let prefix = StableRequestPrefix::new(
        persisted_configuration.selected_identity(),
        persisted_configuration.effective_identity(),
        persisted_configuration
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
        GenerationLimits::new(
            persisted_configuration
                .effective()
                .behavior()
                .output_bound
                .bound(),
            Vec::new(),
        )?,
        NativeOptions::default(),
        Some(plan_identity),
    )?;
    let history_item = ConversationItem::new(
        id::<EntryId>(30)?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new(
            "persist this exact turn",
            None,
        )?)],
        Provenance::UserInput,
    )?;
    let history = CanonicalHistory::new(vec![HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![history_item],
    )?])?;
    let canonical = RuntimeCanonicalState::new(
        prefix,
        history,
        WorkingProjection::default(),
        RuntimeContextState::genesis(id(1)?, id(2)?, [0x5a; 32])?,
    )?;
    let account = BindingReference::new("account-authority", "account-ref", 1)?;
    let profile = BindingReference::new("profile-authority", "profile-ref", 1)?;
    let admission = RevalidationFact::new(
        account.clone(),
        Timestamp::new(10, 0)?,
        Some(Timestamp::new(20, 0)?),
        component(RuntimeComponentKind::AdmissionEvidence, b"admission-v1")?,
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
        component(RuntimeComponentKind::ModelDescriptor, b"descriptor-v1")?,
        admission,
    )?;
    let policy = BindingReference::new("tool-policy-authority", "policy-ref", 1)?;
    let tools = ToolRuntimeManifest::new(
        component(RuntimeComponentKind::ToolRegistry, b"tool-registry-v1")?,
        component(RuntimeComponentKind::ToolExposure, b"tool-exposure-v1")?,
        component(
            RuntimeComponentKind::ExecutionEnvironment,
            b"environment-v1",
        )?,
        component(RuntimeComponentKind::EffectPolicy, b"effect-policy-v1")?,
        policy,
    )?;
    let artifact = FamilyArtifact::new(
        "responses",
        RESPONSES_FAMILY_ARTIFACT_SCHEMA_REVISION,
        RESPONSES_FAMILY_ARTIFACT_CODEC_REVISION,
        digest(DigestDomain::ModelBinding, b"responses-binding-v1")?,
        b"exact-responses-artifact-v1".as_slice(),
    )?;
    let (lifecycle, ledger) = lifecycle_and_ledger.unwrap_or((
        RuntimeLifecycle::Quiescent,
        RuntimeOperationLedger::new(Vec::new(), Vec::new())?,
    ));
    Ok(RuntimeManifest::new(
        provenance,
        RuntimeTopology {
            session_id: id(1)?,
            branch_id: id(2)?,
            checkpoint_id: id(3)?,
            parent_checkpoint_id: Some(id(4)?),
            branch_revision: 5,
            fork: None,
            lease_fence: 6,
            commit_identity: digest(DigestDomain::ProviderOperation, b"commit")?,
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
        JobLedger::default(),
        RuntimeDurability::Required,
    )?)
}

fn quiescent(build: &str) -> TestResult<RuntimeCheckpoint> {
    Ok(RuntimeCheckpoint::new(manifest(
        ProducerProvenance::new("plakous", build, "offline")?,
        None,
    )?)?)
}

#[test]
fn interpreter_observation_is_closed_and_checkpoint_round_trips() -> TestResult {
    assert!(
        InterpreterObservation::new(
            0,
            InterpreterAvailability::Reset,
            Some(InterpreterResetReason::ProcessReopen),
            None,
            None,
        )
        .is_err()
    );
    for (availability, reason, cell, boundary) in [
        (InterpreterAvailability::Available, None, None, None),
        (
            InterpreterAvailability::Available,
            None,
            Some("foreign-cell".to_owned()),
            Some(InterpreterAcknowledgedBoundary::Launch),
        ),
        (
            InterpreterAvailability::Available,
            None,
            None,
            Some(InterpreterAcknowledgedBoundary::SourceAccepted),
        ),
        (
            InterpreterAvailability::Available,
            None,
            None,
            Some(InterpreterAcknowledgedBoundary::Join),
        ),
        (
            InterpreterAvailability::Reset,
            Some(InterpreterResetReason::HostLost),
            Some("joined-cell".to_owned()),
            Some(InterpreterAcknowledgedBoundary::Join),
        ),
    ] {
        assert_eq!(
            InterpreterObservation::new(1, availability, reason, cell, boundary),
            Err(RuntimeManifestError::InvalidInterpreterObservation)
        );
    }
    assert!(
        InterpreterObservation::new(
            1,
            InterpreterAvailability::Available,
            Some(InterpreterResetReason::HostLost),
            None,
            None,
        )
        .is_err()
    );
    let observation = InterpreterObservation::new(
        7,
        InterpreterAvailability::Available,
        None,
        Some("cell-7".to_owned()),
        Some(InterpreterAcknowledgedBoundary::SourceAccepted),
    )?;
    let prior = manifest(
        ProducerProvenance::new("plakous", "observation-build", "offline")?,
        None,
    )?;
    let checkpoint = RuntimeCheckpoint::new(RuntimeManifest::new(
        prior.provenance.clone(),
        prior.topology,
        prior.lifecycle.clone(),
        prior.harness_plan.clone(),
        prior.provider_configuration.clone(),
        prior.canonical.clone(),
        prior.binding.clone(),
        prior.tools.clone(),
        prior.ledger.clone(),
        prior.family_artifact.clone(),
        Some(observation.clone()),
        prior.jobs.clone(),
        prior.durability,
    )?)?;
    let bundle = crate::RuntimeCheckpointCodec::bundle(&checkpoint, None)?;
    let decoded = crate::RuntimeCheckpointCodec::materialize(&bundle)?;
    assert_eq!(
        decoded.manifest().interpreter_observation(),
        Some(&observation)
    );
    assert!(!bundle.objects().iter().any(|object| {
        object
            .canonical_bytes()
            .windows(13)
            .any(|bytes| bytes == b"print-secret!")
    }));
    Ok(())
}

fn working() -> TestResult<RuntimeCheckpoint> {
    let turn_id = id(20)?;
    let operation = DurableOperationRecord::new(
        7,
        DurableOperationKind::Provider {
            root: RuntimeOperationRoot::Turn(turn_id),
            attempt_id: id(21)?,
        },
        component(RuntimeComponentKind::Operation, b"provider-request-v1")?,
        vec![DurableOperationTransition::new(
            id(22)?,
            DurableOperationState::Prepared,
        )],
    )?;
    let lifecycle = RuntimeLifecycle::Working {
        root: RuntimeWorkingRoot::Turn {
            turn_id,
            next_round: 1,
        },
        recovery_phase: WorkingRecoveryPhase::DispatchEvidence,
        obligations: vec![RecoveryObligation::new(7, PROVIDER_HANDLER)],
    };
    let ledger = RuntimeOperationLedger::new(Vec::new(), vec![operation])?;
    Ok(RuntimeCheckpoint::new(manifest(
        ProducerProvenance::new("plakous", "working-build", "offline")?,
        Some((lifecycle, ledger)),
    )?)?)
}

fn witness(manifest: &RuntimeManifest) -> TestResult<RuntimeRevalidationWitness> {
    let binding = manifest.binding();
    let tools = manifest.tools();
    Ok(RuntimeRevalidationWitness::new(
        Timestamp::new(12, 0)?,
        binding.account().clone(),
        binding.profile().clone(),
        binding.model_binding(),
        binding.model_binding(),
        RevalidationFact::new(
            binding.admission().reference().clone(),
            Timestamp::new(11, 0)?,
            Some(Timestamp::new(30, 0)?),
            binding.admission().evidence().clone(),
        )?,
        tools.registry().integrity(),
        tools.exposure().integrity(),
        tools.environment().integrity(),
        tools.effect_policy().integrity(),
        tools.policy().clone(),
    ))
}

fn prepared_operation(operation_id: u64) -> TestResult<DurableOperationRecord> {
    Ok(DurableOperationRecord::new(
        operation_id,
        DurableOperationKind::Command {
            turn_id: Some(id(10_000 + u128::from(operation_id))?),
        },
        component(RuntimeComponentKind::Operation, b"operation-request-v1")?,
        vec![DurableOperationTransition::new(
            id(20_000 + u128::from(operation_id))?,
            DurableOperationState::Prepared,
        )],
    )?)
}

fn command_record(command_id: &str, sequence: u64) -> TestResult<DurableCommandRecord> {
    Ok(DurableCommandRecord::new(
        command_id,
        sequence,
        None,
        digest(DigestDomain::Blob, b"command-binding")?,
        component(RuntimeComponentKind::Command, b"command-v1")?,
        DurableCommandDisposition::Accepted,
        None,
    )?)
}

fn context_history(segment_count: usize) -> TestResult<CanonicalHistory> {
    let segments = (0..segment_count)
        .map(|index| {
            let value = 90_000_u128.saturating_add(index as u128);
            Ok(HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![ConversationItem::new(
                    id::<EntryId>(value)?,
                    Actor::User,
                    vec![ContentBlock::Text(TextBlock::new(
                        format!("context intent {index}"),
                        None,
                    )?)],
                    Provenance::UserInput,
                )?],
            )?)
        })
        .collect::<TestResult<Vec<_>>>()?;
    Ok(CanonicalHistory::new(segments)?)
}

fn compact_context(
    source: &RuntimeContextState,
    source_history: &CanonicalHistory,
    source_checkpoint: CheckpointId,
    entropy: [u8; 32],
) -> TestResult<(RuntimeContextState, CanonicalHistory, HistorySegment)> {
    let summary = HistorySegment::new(
        HistorySegmentKind::Compaction,
        vec![ConversationItem::new(
            id::<EntryId>(190_000 + u128::from(entropy[0]))?,
            Actor::Assistant,
            vec![ContentBlock::Compaction(CompactionBlock::new(
                "bounded semantic summary",
            )?)],
            Provenance::ModelOutput,
        )?],
    )?;
    let projection =
        ConversationView::from_shared(source_history.clone(), WorkingProjection::default());
    let plan = ContextHistoryPlan::new(&projection)?;
    let cut = SemanticCompactionCut::seal(&projection, &plan)?;
    let successor = cut.build_successor(&summary)?;
    let publication = SemanticCompactionPublication::new(
        source_checkpoint,
        digest(
            DigestDomain::ProviderOperation,
            b"semantic-compaction-operation",
        )?,
        digest(DigestDomain::ModelBinding, b"semantic-compaction-route")?,
        digest(DigestDomain::ModelBinding, b"semantic-compaction-model")?,
        digest(
            DigestDomain::ResolvedHarnessPlan,
            b"semantic-compaction-harness",
        )?,
        ContextEvaluationAudit::new(
            digest(DigestDomain::TokenMeasurement, b"measurement-before")?,
            digest(DigestDomain::ContextAdmission, b"status-before")?,
            1_024,
        )?,
        ContextEvaluationAudit::new(
            digest(DigestDomain::TokenMeasurement, b"measurement-after")?,
            digest(DigestDomain::ContextAdmission, b"status-after")?,
            256,
        )?,
        Some(Usage::new(1_024, 64, 8, 512, 0)),
    )?;
    let compacted =
        source.successful_semantic_compaction(&publication, &cut, &summary, &successor, entropy)?;
    Ok((compacted, successor, summary))
}

fn native_compact_context(
    source: &RuntimeContextState,
    source_history: &CanonicalHistory,
    source_checkpoint: CheckpointId,
    entropy: [u8; 32],
) -> TestResult<(RuntimeContextState, CanonicalHistory, HistorySegment)> {
    let summary = HistorySegment::new(
        HistorySegmentKind::Compaction,
        vec![ConversationItem::new(
            id::<EntryId>(180_000 + u128::from(entropy[0]))?,
            Actor::Assistant,
            vec![ContentBlock::Compaction(CompactionBlock::new(
                "native compact summary",
            )?)],
            Provenance::ModelOutput,
        )?],
    )?;
    let mut segments = source_history.segments().to_vec();
    segments.push(summary.clone());
    let successor = CanonicalHistory::new(segments)?;
    let projection =
        ConversationView::from_shared(source_history.clone(), WorkingProjection::default());
    let publication = SemanticCompactionPublication::new(
        source_checkpoint,
        digest(
            DigestDomain::ProviderOperation,
            b"native-compaction-operation",
        )?,
        digest(DigestDomain::ModelBinding, b"native-compaction-route")?,
        digest(DigestDomain::ModelBinding, b"native-compaction-model")?,
        digest(
            DigestDomain::ResolvedHarnessPlan,
            b"native-compaction-harness",
        )?,
        ContextEvaluationAudit::new(
            digest(DigestDomain::TokenMeasurement, b"native-measurement-before")?,
            digest(DigestDomain::ContextAdmission, b"native-status-before")?,
            1_024,
        )?,
        ContextEvaluationAudit::new(
            digest(DigestDomain::TokenMeasurement, b"native-measurement-after")?,
            digest(DigestDomain::ContextAdmission, b"native-status-after")?,
            256,
        )?,
        Some(Usage::new(1_024, 64, 8, 512, 0)),
    )?;
    let compacted = source.successful_native_compaction(
        &publication,
        &summary,
        &successor,
        source_history.identity(),
        projection.identity(),
        entropy,
        source_history.segments().len(),
    )?;
    Ok((compacted, successor, summary))
}

fn later_user_segment(value: u128) -> TestResult<HistorySegment> {
    Ok(HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![ConversationItem::new(
            id::<EntryId>(value)?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new(
                "continue after compact",
                None,
            )?)],
            Provenance::UserInput,
        )?],
    )?)
}

fn empty_request_prefix() -> TestResult<StableRequestPrefix> {
    let configuration = provider_configuration()?;
    Ok(StableRequestPrefix::new(
        configuration.selected_identity(),
        configuration.effective_identity(),
        configuration.effective().behavior().processing_tier.clone(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        OutputPolicy::default(),
        ReasoningPolicy::Disabled,
        ToolChoice::default(),
        CachePolicy::Disabled,
        GenerationLimits::default(),
        NativeOptions::default(),
        None,
    )?)
}

#[test]
fn compaction_provenance_survives_a_later_user_append() -> TestResult {
    let session = id::<SessionId>(83_001)?;
    let branch = id::<BranchId>(83_002)?;
    let source = RuntimeContextState::genesis(session, branch, [0x71; 32])?;
    let prefix = empty_request_prefix()?;
    let history = context_history(3)?;

    let (semantic, semantic_successor, _) =
        compact_context(&source, &history, id(83_003)?, [0x72; 32])?;
    RuntimeCanonicalState::new(
        prefix.clone(),
        semantic_successor.clone(),
        WorkingProjection::default(),
        semantic.clone(),
    )?;
    RuntimeCanonicalState::new(
        prefix.clone(),
        semantic_successor.append(later_user_segment(83_100)?)?,
        WorkingProjection::default(),
        semantic,
    )?;

    let (native, native_successor, _) =
        native_compact_context(&source, &history, id(83_004)?, [0x73; 32])?;
    RuntimeCanonicalState::new(
        prefix.clone(),
        native_successor.clone(),
        WorkingProjection::default(),
        native.clone(),
    )?;
    RuntimeCanonicalState::new(
        prefix,
        native_successor.append(later_user_segment(83_101)?)?,
        WorkingProjection::default(),
        native,
    )?;
    Ok(())
}

#[test]
fn runtime_context_genesis_resume_branch_rotations_and_redaction_are_closed() -> TestResult {
    let session = id::<SessionId>(80_001)?;
    let branch = id::<BranchId>(80_002)?;
    let target_branch = id::<BranchId>(80_003)?;
    let checkpoint = id::<CheckpointId>(80_004)?;
    assert_eq!(
        RuntimeContextState::genesis(session, branch, [0; 32]),
        Err(RuntimeManifestError::InvalidContextState)
    );
    let source = RuntimeContextState::genesis(session, branch, [0x41; 32])?;
    assert_eq!(source.affinity().epoch(), 0);
    source.validate_binding(session, branch)?;
    assert_eq!(
        source.validate_binding(session, target_branch),
        Err(RuntimeManifestError::InvalidContextState)
    );
    assert_eq!(source.retained_for_exact_resume(), source);
    assert!(!format!("{source:?}").contains("4141414141414141"));
    assert!(!format!("{:?}", source.affinity()).contains("4141414141414141"));

    let fork = source.ordinary_fork(checkpoint, session, target_branch, [0x42; 32])?;
    assert_ne!(fork.affinity().identity(), source.affinity().identity());
    assert_eq!(fork.affinity().epoch(), 0);
    assert!(fork.occupancy_anchor().is_none());
    fork.validate_branch_transition_from(
        &source,
        checkpoint,
        RuntimeContextTransitionKind::OrdinaryFork,
    )?;
    assert_eq!(
        fork.validate_branch_transition_from(
            &source,
            checkpoint,
            RuntimeContextTransitionKind::Rebind,
        ),
        Err(RuntimeManifestError::InvalidContextTransition)
    );
    assert_eq!(
        fork.validate_branch_transition_from(
            &source,
            id(80_005)?,
            RuntimeContextTransitionKind::OrdinaryFork,
        ),
        Err(RuntimeManifestError::InvalidContextTransition)
    );
    let other_source = RuntimeContextState::genesis(session, branch, [0x43; 32])?;
    assert_eq!(
        fork.validate_branch_transition_from(
            &other_source,
            checkpoint,
            RuntimeContextTransitionKind::OrdinaryFork,
        ),
        Err(RuntimeManifestError::InvalidContextTransition)
    );
    assert_eq!(
        source.ordinary_fork(checkpoint, session, branch, [0x44; 32]),
        Err(RuntimeManifestError::InvalidContextTransition)
    );

    let rebind_branch = id::<BranchId>(80_006)?;
    let rebound = source.rebind(checkpoint, session, rebind_branch, [0x45; 32])?;
    rebound.validate_branch_transition_from(
        &source,
        checkpoint,
        RuntimeContextTransitionKind::Rebind,
    )?;
    assert_ne!(rebound.affinity().identity(), source.affinity().identity());
    assert!(rebound.occupancy_anchor().is_none());
    Ok(())
}

#[test]
fn runtime_context_codec_recomputes_identities_and_is_bounded() -> TestResult {
    let state = RuntimeContextState::genesis(id(81_001)?, id(81_002)?, [0x5a; 32])?;
    let encoded = state.encode()?;
    assert_eq!(RuntimeContextState::decode(&encoded)?, state);
    assert!(encoded.len() < MAX_RUNTIME_CONTEXT_STATE_BYTES);

    let mut tampered = encoded.clone();
    tampered[16] ^= 1;
    assert_eq!(
        RuntimeContextState::decode(&tampered),
        Err(RuntimeManifestError::IntegrityMismatch)
    );
    let body = decode_envelope(
        &encoded,
        CONTEXT_STATE_MAGIC,
        RUNTIME_CONTEXT_STATE_CODEC_REVISION,
        MAX_RUNTIME_CONTEXT_STATE_BYTES,
        b"runtime-context-state",
    )?;
    let mut forged_identity = body.to_vec();
    let last = forged_identity
        .last_mut()
        .ok_or("context codec body must not be empty")?;
    *last ^= 1;
    let forged = encode_envelope(
        CONTEXT_STATE_MAGIC,
        RUNTIME_CONTEXT_STATE_CODEC_REVISION,
        &forged_identity,
        root_integrity(b"runtime-context-state", &forged_identity)?,
        MAX_RUNTIME_CONTEXT_STATE_BYTES,
    )?;
    assert_eq!(
        RuntimeContextState::decode(&forged),
        Err(RuntimeManifestError::IntegrityMismatch)
    );
    let mut trailing = encoded;
    trailing.push(0);
    assert_eq!(
        RuntimeContextState::decode(&trailing),
        Err(RuntimeManifestError::TrailingBytes)
    );
    assert_eq!(
        RuntimeContextState::decode(&vec![0; MAX_RUNTIME_CONTEXT_STATE_BYTES + 1]),
        Err(RuntimeManifestError::EnvelopeSize)
    );
    Ok(())
}

/// The quiescent checkpoint carries the driver's context-measure anchor so a
/// plain reopen keeps it; it is part of the state identity and codec, and no
/// branch transition or compaction successor inherits it.
#[test]
fn occupancy_anchor_round_trips_and_no_successor_inherits_it() -> TestResult {
    let session = id::<SessionId>(81_101)?;
    let branch = id::<BranchId>(81_102)?;
    let checkpoint = id::<CheckpointId>(81_103)?;
    let source = RuntimeContextState::genesis(session, branch, [0x5b; 32])?;
    assert_eq!(source.occupancy_anchor(), None);
    let anchor = ContextAnchor::new(298_468, 850_715);
    let request_anchor = plakous_model::ContextRequestAnchor::new(
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"fixture-binding"])?,
        RouteId::new("fixture-route")?,
        plakous_model::ModelId::new("fixture-model")?,
        Usage::new(298_468, 1000, 0, 100_000, 20_000),
        plakous_model::ContextWireInput::measure_wire(
            br#"{"messages":[{"role":"user","content":"q"}]}"#,
            None,
        )?,
    )
    .ok_or("anchor")?;
    let anchored = source
        .with_occupancy_anchor(anchor)?
        .with_request_anchor(Some(request_anchor.clone()))?;
    assert_eq!(anchored.occupancy_anchor(), anchor);
    assert_ne!(anchored.identity(), source.identity());
    assert_eq!(RuntimeContextState::decode(&anchored.encode()?)?, anchored);
    assert_eq!(anchored.request_anchor(), Some(&request_anchor));
    assert_eq!(
        anchored
            .with_occupancy_anchor(None)?
            .with_request_anchor(None)?,
        source
    );

    let fork = anchored.ordinary_fork(checkpoint, session, id(81_104)?, [0x5c; 32])?;
    assert_eq!(fork.occupancy_anchor(), None);
    assert_eq!(fork.request_anchor(), None);
    fork.validate_branch_transition_from(
        &anchored,
        checkpoint,
        RuntimeContextTransitionKind::OrdinaryFork,
    )?;
    let rebound = anchored.rebind(checkpoint, session, id(81_105)?, [0x5d; 32])?;
    assert_eq!(rebound.occupancy_anchor(), None);
    assert_eq!(rebound.request_anchor(), None);
    assert_eq!(
        fork.with_occupancy_anchor(anchor)?
            .validate_branch_transition_from(
                &anchored,
                checkpoint,
                RuntimeContextTransitionKind::OrdinaryFork,
            ),
        Err(RuntimeManifestError::InvalidContextTransition),
        "a branch successor never carries an anchor"
    );
    let (compacted, _, _) =
        compact_context(&anchored, &context_history(4)?, id(81_106)?, [0x5e; 32])?;
    assert_eq!(compacted.occupancy_anchor(), None);
    assert_eq!(compacted.request_anchor(), None);
    Ok(())
}

#[test]
fn paid_compaction_provenance_rejects_preserved_cache_affinity() -> TestResult {
    let source = RuntimeContextState::genesis(id(82_101)?, id(82_102)?, [0x61; 32])?;
    let (compacted, _, _) =
        compact_context(&source, &context_history(4)?, id(82_103)?, [0x62; 32])?;
    let mut provenance = compacted
        .compaction_provenance()
        .ok_or("compaction provenance")?
        .clone();
    provenance.invalidation.target_affinity = provenance.invalidation.source_affinity;
    provenance.invalidation.target_epoch = provenance.invalidation.source_epoch;
    assert_eq!(
        provenance.validate(),
        Err(RuntimeManifestError::InvalidCompactionProvenance)
    );
    Ok(())
}

#[test]
fn semantic_compaction_rotates_epoch_clears_anchors_and_stays_constant_sized() -> TestResult {
    let session = id::<SessionId>(82_001)?;
    let branch = id::<BranchId>(82_002)?;
    let source = RuntimeContextState::genesis(session, branch, [0x61; 32])?;
    let small_history = context_history(4)?;
    let (small, small_successor, small_summary) =
        compact_context(&source, &small_history, id(82_003)?, [0x62; 32])?;
    assert_eq!(small.affinity().epoch(), 1);
    assert_ne!(small.affinity().identity(), source.affinity().identity());
    assert!(small.occupancy_anchor().is_none());
    assert_eq!(
        small.transition().map(RuntimeContextTransition::kind),
        Some(RuntimeContextTransitionKind::SuccessfulCompaction)
    );
    let provenance = small
        .compaction_provenance()
        .ok_or("compaction provenance")?;
    assert_eq!(
        provenance.summary_segment_identity(),
        small_summary.identity()
    );
    assert!(provenance.invalidation().clears_capacity_observations());
    assert!(provenance.invalidation().clears_continuation_anchor());
    assert_eq!(RuntimeContextState::decode(&small.encode()?)?, small);
    let configuration = provider_configuration()?;
    RuntimeCanonicalState::new(
        StableRequestPrefix::new(
            configuration.selected_identity(),
            configuration.effective_identity(),
            configuration.effective().behavior().processing_tier.clone(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            OutputPolicy::default(),
            ReasoningPolicy::Disabled,
            ToolChoice::default(),
            CachePolicy::Disabled,
            GenerationLimits::default(),
            NativeOptions::default(),
            None,
        )?,
        small_successor,
        WorkingProjection::default(),
        small.clone(),
    )?;

    let large_history = context_history(256)?;
    let (large, _, _) = compact_context(&source, &large_history, id(82_004)?, [0x63; 32])?;
    let small_size = small.encode()?.len();
    let large_size = large.encode()?.len();
    assert!(small_size.abs_diff(large_size) <= 16);
    assert!(large_size < 2 * 1024);

    let not_summary = small_history.segments()[0].clone();
    let small_projection =
        ConversationView::from_shared(small_history.clone(), WorkingProjection::default());
    let small_cut = SemanticCompactionCut::seal(
        &small_projection,
        &ContextHistoryPlan::new(&small_projection)?,
    )?;
    assert_eq!(
        source.successful_semantic_compaction(
            &SemanticCompactionPublication::new(
                id(82_005)?,
                digest(DigestDomain::ProviderOperation, b"bad-compaction-operation")?,
                digest(DigestDomain::ModelBinding, b"bad-compaction-route")?,
                digest(DigestDomain::ModelBinding, b"bad-compaction-model")?,
                digest(DigestDomain::ResolvedHarnessPlan, b"bad-compaction-harness")?,
                ContextEvaluationAudit::new(
                    digest(DigestDomain::TokenMeasurement, b"bad-before-measurement")?,
                    digest(DigestDomain::ContextAdmission, b"bad-before-status")?,
                    10,
                )?,
                ContextEvaluationAudit::new(
                    digest(DigestDomain::TokenMeasurement, b"bad-after-measurement")?,
                    digest(DigestDomain::ContextAdmission, b"bad-after-status")?,
                    5,
                )?,
                None,
            )?,
            &small_cut,
            &not_summary,
            &large_history,
            [0x64; 32],
        ),
        Err(RuntimeManifestError::InvalidCompactionProvenance)
    );
    Ok(())
}

#[test]
fn operation_avl_shares_unaffected_subtrees_and_replays_every_mutation() -> TestResult {
    let operations = (1..=7)
        .map(prepared_operation)
        .collect::<TestResult<Vec<_>>>()?;
    let base = RuntimeOperationLedger::new(Vec::new(), operations)?;
    let appended = base
        .append_operation(prepared_operation(8)?)?
        .append_operation(prepared_operation(9)?)?;
    let updated = appended.transition_operation(
        2,
        DurableOperationTransition::new(id(30_002)?, DurableOperationState::Dispatched),
    )?;
    let before_root = appended
        .operation_root
        .as_ref()
        .expect("nonempty operation map");
    let after_root = updated
        .operation_root
        .as_ref()
        .expect("nonempty operation map");
    assert!(Arc::ptr_eq(
        before_root.right.as_ref().expect("right subtree"),
        after_root.right.as_ref().expect("right subtree")
    ));
    let updated = updated.transition_operation(
        8,
        DurableOperationTransition::new(id(30_008)?, DurableOperationState::Dispatched),
    )?;

    let delta = updated
        .operation_delta_since(&base)
        .expect("base is an exact pointer ancestor");
    assert_eq!(
        delta
            .iter()
            .copied()
            .map(|mutation| (mutation.inserted(), mutation.record().operation_id()))
            .collect::<Vec<_>>(),
        vec![(true, 8), (true, 9), (false, 2), (false, 8)]
    );
    assert_eq!(
        updated
            .operations()
            .into_iter()
            .map(DurableOperationRecord::operation_id)
            .collect::<Vec<_>>(),
        (1..=9).collect::<Vec<_>>()
    );

    let semantic_base =
        RuntimeOperationLedger::new(Vec::new(), base.operations().into_iter().cloned().collect())?;
    assert_eq!(
        updated
            .operation_delta_since(&semantic_base)
            .expect("authenticated semantic ancestor")
            .len(),
        4
    );

    let current =
        operation_map_get(updated.operation_root.as_deref(), 2).expect("updated operation exists");
    let invalid_prefix = DurableOperationRecord::new(
        2,
        current.kind.clone(),
        current.request.clone(),
        vec![
            DurableOperationTransition::new(id(40_002)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(id(40_003)?, DurableOperationState::Dispatched),
        ],
    )?;
    assert_eq!(
        updated.replace_operation(invalid_prefix),
        Err(RuntimeManifestError::InvalidLedger)
    );
    assert_eq!(updated.unresolved_operation_count(), 9);
    let updated = updated.transition_operation(
        2,
        DurableOperationTransition::new(
            id(50_002)?,
            DurableOperationState::Completed(component(
                RuntimeComponentKind::Operation,
                b"operation-outcome-v1",
            )?),
        ),
    )?;
    let updated = updated.transition_operation(
        2,
        DurableOperationTransition::new(
            id(50_003)?,
            DurableOperationState::Finalized(component(
                RuntimeComponentKind::Operation,
                b"operation-outcome-v1",
            )?),
        ),
    )?;
    assert_eq!(updated.unresolved_operation_count(), 8);
    Ok(())
}

#[test]
fn process_session_identity_kind_and_closed_lifetime_round_trip() -> TestResult {
    assert_eq!(
        ProcessSessionId::new([0; PROCESS_SESSION_ID_BYTES]),
        Err(RuntimeManifestError::InvalidLedger)
    );
    assert_eq!(
        ProcessSessionOwnerGeneration::new(0),
        Err(RuntimeManifestError::InvalidLedger)
    );
    let turn_id = id(89_001)?;
    let session_id = ProcessSessionId::new([0x89; PROCESS_SESSION_ID_BYTES])?;
    let owner_generation = ProcessSessionOwnerGeneration::new(7)?;
    let request = component(
        RuntimeComponentKind::Operation,
        b"joined-process-lifetime-v1",
    )?;
    let joined = component(RuntimeComponentKind::Operation, b"joined-proof-v1")?;
    let record = DurableOperationRecord::new(
        89,
        DurableOperationKind::ProcessSession {
            turn_id,
            session_id,
            owner_generation,
        },
        request.clone(),
        vec![
            DurableOperationTransition::new(id(89_002)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(id(89_003)?, DurableOperationState::Dispatched),
            DurableOperationTransition::new(
                id(89_004)?,
                DurableOperationState::Completed(joined.clone()),
            ),
            DurableOperationTransition::new(
                id(89_005)?,
                DurableOperationState::Finalized(joined.clone()),
            ),
        ],
    )?;
    assert_operation_round_trips(&record)?;

    for transitions in [
        vec![
            DurableOperationTransition::new(id(89_006)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(
                id(89_007)?,
                DurableOperationState::Completed(joined.clone()),
            ),
        ],
        vec![
            DurableOperationTransition::new(id(89_008)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(
                id(89_009)?,
                DurableOperationState::Claimed(joined.clone()),
            ),
        ],
        vec![
            DurableOperationTransition::new(id(89_010)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(
                id(89_011)?,
                DurableOperationState::OutcomeUnknown(joined.clone()),
            ),
        ],
    ] {
        assert_eq!(
            DurableOperationRecord::new(
                90,
                DurableOperationKind::ProcessSession {
                    turn_id,
                    session_id,
                    owner_generation,
                },
                request.clone(),
                transitions,
            ),
            Err(RuntimeManifestError::InvalidLedger)
        );
    }

    let duplicate = DurableOperationRecord::new(
        90,
        DurableOperationKind::ProcessSession {
            turn_id,
            session_id,
            owner_generation: ProcessSessionOwnerGeneration::new(8)?,
        },
        request,
        vec![DurableOperationTransition::new(
            id(89_012)?,
            DurableOperationState::Prepared,
        )],
    )?;
    let ledger = RuntimeOperationLedger::new(Vec::new(), vec![record])?;
    assert_eq!(
        ledger.append_operation(duplicate),
        Err(RuntimeManifestError::InvalidLedger)
    );
    Ok(())
}

#[test]
fn composition_evaluation_locator_round_trips_without_source_replay_authority() -> TestResult {
    let turn_id = id(89_021)?;
    let request = component(
        RuntimeComponentKind::Operation,
        b"source-digest/start-fence/cell/nested-sequence/child-refs/cleanup-v1",
    )?;
    let locator = component(
        RuntimeComponentKind::Operation,
        b"cell-7/sequence-2/children-11,12/active",
    )?;
    let record = DurableOperationRecord::new(
        91,
        DurableOperationKind::CompositionEvaluation {
            turn_id,
            attempt_id: id(89_022)?,
            call_id: id(89_023)?,
            source_index: 3,
        },
        request,
        vec![
            DurableOperationTransition::new(id(89_024)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(id(89_025)?, DurableOperationState::Dispatched),
            DurableOperationTransition::new(
                id(89_026)?,
                DurableOperationState::LocatorBound(locator),
            ),
        ],
    )?;
    assert_operation_round_trips(&record)?;
    assert_eq!(
        record.kind().obligation_kind(),
        RecoveryObligationKind::CodeCell
    );
    Ok(())
}

#[test]
fn composition_child_round_trip_binds_parent_sequence_and_canonical_call() -> TestResult {
    let turn_id = id(89_031)?;
    let record = DurableOperationRecord::new(
        92,
        DurableOperationKind::CompositionChild {
            turn_id,
            attempt_id: id(89_032)?,
            call_id: id(89_033)?,
            evaluation_operation_id: 91,
            interpreter_generation: 4,
            cell_id: "cell-4".to_owned(),
            request_sequence: 7,
        },
        component(
            RuntimeComponentKind::Operation,
            b"parent-91/sequence-7/runtime-call/digest/input-v1",
        )?,
        vec![
            DurableOperationTransition::new(id(89_034)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(id(89_035)?, DurableOperationState::Dispatched),
            DurableOperationTransition::new(
                id(89_036)?,
                DurableOperationState::Completed(component(
                    RuntimeComponentKind::Operation,
                    b"canonical-child-outcome-v1",
                )?),
            ),
        ],
    )?;
    assert_operation_round_trips(&record)?;
    assert_eq!(
        record.kind().obligation_kind(),
        RecoveryObligationKind::CodeCell
    );
    for kind in [
        DurableOperationKind::CompositionChild {
            turn_id,
            attempt_id: id(89_032)?,
            call_id: id(89_033)?,
            evaluation_operation_id: 91,
            interpreter_generation: 0,
            cell_id: "cell-4".to_owned(),
            request_sequence: 7,
        },
        DurableOperationKind::CompositionChild {
            turn_id,
            attempt_id: id(89_032)?,
            call_id: id(89_033)?,
            evaluation_operation_id: 91,
            interpreter_generation: 4,
            cell_id: String::new(),
            request_sequence: 7,
        },
    ] {
        assert_eq!(
            DurableOperationRecord::new(
                93,
                kind,
                component(RuntimeComponentKind::Operation, b"invalid-child")?,
                vec![DurableOperationTransition::new(
                    id(89_037)?,
                    DurableOperationState::Prepared,
                )],
            ),
            Err(RuntimeManifestError::InvalidLedger)
        );
    }
    Ok(())
}

fn composition_call(call_id: CallId, tool: &str) -> TestResult<ToolCallBlock> {
    Ok(ToolCallBlock::new(
        call_id,
        ToolName::new(tool)?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::Direct)?,
        "{}",
    )?)
}

fn finalized_composition_transitions(seed: u128) -> TestResult<Vec<DurableOperationTransition>> {
    let evidence = component(RuntimeComponentKind::Operation, b"composition-outcome-v1")?;
    Ok(vec![
        DurableOperationTransition::new(id(seed)?, DurableOperationState::Prepared),
        DurableOperationTransition::new(id(seed + 1)?, DurableOperationState::Dispatched),
        DurableOperationTransition::new(
            id(seed + 2)?,
            DurableOperationState::Completed(evidence.clone()),
        ),
        DurableOperationTransition::new(id(seed + 3)?, DurableOperationState::Finalized(evidence)),
    ])
}

fn composition_graph_records() -> TestResult<Vec<DurableOperationRecord>> {
    let turn_id = id(89_201)?;
    let attempt_id = id(89_202)?;
    let evaluation_call_id = id(89_203)?;
    let evaluation_request = CompositionEvaluationRequest {
        version: 2,
        descriptor_identity: digest(DigestDomain::ProviderOperation, b"descriptor")?,
        host_identity: b"host-v1".to_vec(),
        call: composition_call(evaluation_call_id, "exec")?,
        arguments_digest: digest(DigestDomain::ProviderOperation, b"arguments")?,
        source_digest: Some(digest(DigestDomain::Blob, b"source")?),
        result_entry_id: id(89_204)?,
    };
    let evaluation = DurableOperationRecord::new(
        101,
        DurableOperationKind::CompositionEvaluation {
            turn_id,
            attempt_id,
            call_id: evaluation_call_id,
            source_index: 1,
        },
        ReconstructiveComponent::new(
            RuntimeComponentKind::Operation,
            1,
            serde_json::to_vec(&(
                "plakous-local-composition-evaluation-v2",
                evaluation_request,
            ))?,
        )?,
        finalized_composition_transitions(89_210)?,
    )?;

    let continuation_call_id = id(89_220)?;
    let continuation_request = CompositionContinuationRequest {
        version: 1,
        descriptor_identity: digest(DigestDomain::ProviderOperation, b"descriptor")?,
        host_identity: b"host-v1".to_vec(),
        evaluation_operation_id: 101,
        call: composition_call(continuation_call_id, "wait")?,
        arguments_digest: digest(DigestDomain::ProviderOperation, b"wait-arguments")?,
        result_entry_id: id(89_221)?,
    };
    let continuation = DurableOperationRecord::new(
        102,
        DurableOperationKind::CompositionContinuation {
            turn_id,
            attempt_id,
            call_id: continuation_call_id,
            source_index: 2,
            evaluation_operation_id: 101,
        },
        ReconstructiveComponent::new(
            RuntimeComponentKind::Operation,
            1,
            serde_json::to_vec(&(
                "plakous-local-composition-continuation-v1",
                continuation_request,
            ))?,
        )?,
        finalized_composition_transitions(89_230)?,
    )?;

    let cell_id = "cell-101";
    let nested_request = serde_json::to_vec(&(
        "plakous-local-composition-child-request-v1",
        cell_id,
        "runtime-call-1",
        "read_file",
        &Some(serde_json::json!({"path": "README.md"})),
    ))?;
    let request_digest = Digest::sha256(
        DigestDomain::ProviderOperation,
        3,
        &[
            b"plakous-agent/local-composition-child-request-v1",
            &nested_request,
        ],
    )?;
    let edge_request = serde_json::to_vec(&(
        "plakous-local-composition-child-edge-v2",
        101_u64,
        7_u64,
        cell_id,
        1_u32,
        request_digest,
        &nested_request,
    ))?;
    let edge_binding = Digest::sha256(
        DigestDomain::ProviderOperation,
        4,
        &[
            b"plakous-agent/local-composition-child-edge-v2",
            &edge_request,
        ],
    )?;
    let child = DurableOperationRecord::new(
        103,
        DurableOperationKind::CompositionChild {
            turn_id,
            attempt_id,
            call_id: composition_child_call_id(edge_binding)?,
            evaluation_operation_id: 101,
            interpreter_generation: 7,
            cell_id: cell_id.to_owned(),
            request_sequence: 1,
        },
        ReconstructiveComponent::new(RuntimeComponentKind::Operation, 1, edge_request)?,
        finalized_composition_transitions(89_240)?,
    )?;
    Ok(vec![evaluation, continuation, child])
}

fn composition_continuation_with_request(
    record: &DurableOperationRecord,
    request: CompositionContinuationRequest,
) -> TestResult<DurableOperationRecord> {
    Ok(DurableOperationRecord::new(
        record.operation_id(),
        record.kind().clone(),
        ReconstructiveComponent::new(
            RuntimeComponentKind::Operation,
            1,
            serde_json::to_vec(&("plakous-local-composition-continuation-v1", request))?,
        )?,
        record.transitions().to_vec(),
    )?)
}

#[test]
fn whole_composition_graph_round_trips_through_checkpoint_materialization() -> TestResult {
    let ledger = RuntimeOperationLedger::new(Vec::new(), composition_graph_records()?)?;
    validate_composition_graph(&ledger)?;
    let checkpoint = RuntimeCheckpoint::new(manifest(
        ProducerProvenance::new("plakous", "composition-graph-build", "offline")?,
        Some((RuntimeLifecycle::Quiescent, ledger)),
    )?)?;
    let bundle = crate::RuntimeCheckpointCodec::bundle(&checkpoint, None)?;
    let decoded = crate::RuntimeCheckpointCodec::materialize(&bundle)?;
    assert_eq!(decoded.manifest().ledger(), checkpoint.manifest().ledger());
    Ok(())
}

#[test]
fn whole_composition_graph_rejects_foreign_continuation_descriptor() -> TestResult {
    let records = composition_graph_records()?;
    let mut request: CompositionContinuationRequest = decode_exact_composition_request(
        records[1].request().bytes(),
        "plakous-local-composition-continuation-v1",
    )?;
    request.descriptor_identity = digest(DigestDomain::ProviderOperation, b"foreign-descriptor")?;
    let continuation = composition_continuation_with_request(&records[1], request)?;
    let ledger = RuntimeOperationLedger::new(Vec::new(), vec![records[0].clone(), continuation])?;

    assert_eq!(
        validate_composition_graph(&ledger),
        Err(RuntimeManifestError::InvalidLedger)
    );
    Ok(())
}

#[test]
fn whole_composition_graph_rejects_foreign_continuation_host() -> TestResult {
    let records = composition_graph_records()?;
    let mut request: CompositionContinuationRequest = decode_exact_composition_request(
        records[1].request().bytes(),
        "plakous-local-composition-continuation-v1",
    )?;
    request.host_identity = b"foreign-host".to_vec();
    let continuation = composition_continuation_with_request(&records[1], request)?;
    let ledger = RuntimeOperationLedger::new(Vec::new(), vec![records[0].clone(), continuation])?;

    assert_eq!(
        validate_composition_graph(&ledger),
        Err(RuntimeManifestError::InvalidLedger)
    );
    Ok(())
}

#[test]
fn whole_composition_graph_rejects_foreign_and_contradictory_edges() -> TestResult {
    let records = composition_graph_records()?;
    let evaluation = records[0].clone();
    let continuation = &records[1];
    let child = &records[2];

    let foreign_continuation = DurableOperationRecord::new(
        continuation.operation_id(),
        match continuation.kind() {
            DurableOperationKind::CompositionContinuation {
                turn_id,
                attempt_id,
                call_id,
                source_index,
                ..
            } => DurableOperationKind::CompositionContinuation {
                turn_id: *turn_id,
                attempt_id: *attempt_id,
                call_id: *call_id,
                source_index: *source_index,
                evaluation_operation_id: 999,
            },
            _ => unreachable!(),
        },
        continuation.request().clone(),
        continuation.transitions().to_vec(),
    )?;
    let ledger =
        RuntimeOperationLedger::new(Vec::new(), vec![evaluation.clone(), foreign_continuation])?;
    assert_eq!(
        validate_composition_graph(&ledger),
        Err(RuntimeManifestError::InvalidLedger)
    );

    let wrong_sequence = DurableOperationRecord::new(
        child.operation_id(),
        match child.kind() {
            DurableOperationKind::CompositionChild {
                turn_id,
                attempt_id,
                call_id,
                evaluation_operation_id,
                interpreter_generation,
                cell_id,
                ..
            } => DurableOperationKind::CompositionChild {
                turn_id: *turn_id,
                attempt_id: *attempt_id,
                call_id: *call_id,
                evaluation_operation_id: *evaluation_operation_id,
                interpreter_generation: *interpreter_generation,
                cell_id: cell_id.clone(),
                request_sequence: 2,
            },
            _ => unreachable!(),
        },
        child.request().clone(),
        child.transitions().to_vec(),
    )?;
    let ledger = RuntimeOperationLedger::new(Vec::new(), vec![evaluation, wrong_sequence])?;
    assert_eq!(
        validate_composition_graph(&ledger),
        Err(RuntimeManifestError::InvalidLedger)
    );
    Ok(())
}

#[test]
fn process_session_working_obligation_is_effect_evidence_only() -> TestResult {
    let turn_id = id(89_101)?;
    let operation = DurableOperationRecord::new(
        89,
        DurableOperationKind::ProcessSession {
            turn_id,
            session_id: ProcessSessionId::new([0x8a; PROCESS_SESSION_ID_BYTES])?,
            owner_generation: ProcessSessionOwnerGeneration::new(11)?,
        },
        component(
            RuntimeComponentKind::Operation,
            b"active-process-session-v1",
        )?,
        vec![
            DurableOperationTransition::new(id(89_102)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(id(89_103)?, DurableOperationState::Dispatched),
        ],
    )?;
    let lifecycle = RuntimeLifecycle::Working {
        root: RuntimeWorkingRoot::Turn {
            turn_id,
            next_round: 2,
        },
        recovery_phase: WorkingRecoveryPhase::EffectReconciliation,
        obligations: vec![RecoveryObligation::new(
            89,
            RecoveryHandlerSignature::new(
                RecoveryObligationKind::ProcessSession,
                RecoveryAction::EffectEvidenceRequired,
            ),
        )],
    };
    let recovered = manifest(
        ProducerProvenance::new("plakous", "process-session-working", "offline")?,
        Some((
            lifecycle,
            RuntimeOperationLedger::new(Vec::new(), vec![operation.clone()])?,
        )),
    )?;
    assert_eq!(
        recovered.lifecycle().checkpoint_kind(),
        RuntimeCheckpointKind::Working
    );

    let forbidden = RuntimeLifecycle::Working {
        root: RuntimeWorkingRoot::Turn {
            turn_id,
            next_round: 2,
        },
        recovery_phase: WorkingRecoveryPhase::DispatchEvidence,
        obligations: vec![RecoveryObligation::new(
            89,
            RecoveryHandlerSignature::new(
                RecoveryObligationKind::ProcessSession,
                RecoveryAction::ProvenNotDispatched,
            ),
        )],
    };
    let error = manifest(
        ProducerProvenance::new("plakous", "process-session-no-handler", "offline")?,
        Some((
            forbidden,
            RuntimeOperationLedger::new(Vec::new(), vec![operation])?,
        )),
    )
    .expect_err("an active direct-host session has no non-dispatch recovery handler");
    assert_eq!(
        error.downcast_ref::<RuntimeManifestError>(),
        Some(&RuntimeManifestError::InvalidLifecycle)
    );
    Ok(())
}

#[test]
fn only_commands_may_be_atomically_recorded_as_finalized() -> TestResult {
    let evidence = component(
        RuntimeComponentKind::Operation,
        b"atomic-command-finalization-v1",
    )?;
    let transition = DurableOperationTransition::new(
        id(90_001)?,
        DurableOperationState::Finalized(evidence.clone()),
    );
    let command = DurableOperationRecord::new(
        90,
        DurableOperationKind::Command {
            turn_id: Some(id(90)?),
        },
        component(
            RuntimeComponentKind::Operation,
            b"atomic-command-request-v1",
        )?,
        vec![transition.clone()],
    )?;
    assert!(matches!(
        command.state(),
        DurableOperationState::Finalized(_)
    ));
    let prepared = DurableOperationTransition::new(id(90_002)?, DurableOperationState::Prepared);
    let command = DurableOperationRecord::new(
        91,
        DurableOperationKind::Command {
            turn_id: Some(id(91)?),
        },
        component(
            RuntimeComponentKind::Operation,
            b"atomic-command-request-v2",
        )?,
        vec![
            prepared.clone(),
            DurableOperationTransition::new(
                id(90_003)?,
                DurableOperationState::Finalized(evidence.clone()),
            ),
        ],
    )?;
    assert!(matches!(
        command.state(),
        DurableOperationState::Finalized(_)
    ));
    assert_eq!(
        DurableOperationRecord::new(
            92,
            DurableOperationKind::Provider {
                root: ModelOperationRoot::Turn(id(92)?),
                attempt_id: id(93)?,
            },
            component(RuntimeComponentKind::Operation, b"provider-request-v1")?,
            vec![transition],
        ),
        Err(RuntimeManifestError::InvalidLedger)
    );
    assert_eq!(
        DurableOperationRecord::new(
            93,
            DurableOperationKind::Effect {
                turn_id: id(93)?,
                attempt_id: id(94)?,
                call_id: id(95)?,
                source_index: 0,
            },
            component(RuntimeComponentKind::Operation, b"effect-request-v1")?,
            vec![
                prepared,
                DurableOperationTransition::new(
                    id(90_004)?,
                    DurableOperationState::Finalized(evidence),
                ),
            ],
        ),
        Err(RuntimeManifestError::InvalidLedger)
    );
    Ok(())
}

#[test]
fn same_checkpoint_finalization_cannot_replace_terminal_evidence() -> TestResult {
    let checkpoint = id(90_010)?;
    let terminal = component(RuntimeComponentKind::Operation, b"terminal-evidence-v1")?;
    let contradictory = component(
        RuntimeComponentKind::Operation,
        b"caller-selected-final-evidence-v1",
    )?;
    let transitions = vec![
        DurableOperationTransition::new(id(90_009)?, DurableOperationState::Prepared),
        DurableOperationTransition::new(id(90_008)?, DurableOperationState::Dispatched),
        DurableOperationTransition::new(
            checkpoint,
            DurableOperationState::Completed(terminal.clone()),
        ),
        DurableOperationTransition::new(
            checkpoint,
            DurableOperationState::Finalized(contradictory),
        ),
    ];
    assert_eq!(
        DurableOperationRecord::new(
            90_011,
            DurableOperationKind::Provider {
                root: ModelOperationRoot::Turn(id(90_012)?),
                attempt_id: id(90_013)?,
            },
            component(RuntimeComponentKind::Operation, b"provider-request-v2")?,
            transitions,
        ),
        Err(RuntimeManifestError::InvalidLedger)
    );
    let accepted = DurableOperationRecord::new(
        90_014,
        DurableOperationKind::Provider {
            root: ModelOperationRoot::Turn(id(90_015)?),
            attempt_id: id(90_016)?,
        },
        component(RuntimeComponentKind::Operation, b"provider-request-v3")?,
        vec![
            DurableOperationTransition::new(id(90_017)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(id(90_018)?, DurableOperationState::Dispatched),
            DurableOperationTransition::new(
                checkpoint,
                DurableOperationState::Completed(terminal.clone()),
            ),
            DurableOperationTransition::new(checkpoint, DurableOperationState::Finalized(terminal)),
        ],
    )?;
    assert!(matches!(
        accepted.state(),
        DurableOperationState::Finalized(_)
    ));
    Ok(())
}

#[test]
fn hosted_continuations_have_no_unrecoverable_published_state() -> TestResult {
    let turn_id = id(90_100)?;
    let attempt_id = id(90_101)?;
    for kind in [
        DurableOperationKind::ToolBatchContinuation {
            turn_id,
            attempt_id,
        },
        DurableOperationKind::ServerPauseContinuation {
            turn_id,
            attempt_id,
        },
    ] {
        let request = component(RuntimeComponentKind::Operation, b"hosted-continuation-v1")?;
        let restorable = DurableOperationTransition::new(
            id(90_102)?,
            DurableOperationState::ContinuationRequired(request.clone()),
        );
        let finalized = DurableOperationTransition::new(
            id(90_103)?,
            DurableOperationState::Finalized(request.clone()),
        );
        assert!(
            DurableOperationRecord::new(
                90_104,
                kind.clone(),
                request.clone(),
                vec![restorable.clone()],
            )
            .is_ok()
        );
        assert!(
            DurableOperationRecord::new(
                90_105,
                kind.clone(),
                request.clone(),
                vec![restorable.clone(), finalized],
            )
            .is_ok()
        );
        for forbidden in [
            DurableOperationState::Prepared,
            DurableOperationState::Completed(request.clone()),
        ] {
            assert_eq!(
                DurableOperationRecord::new(
                    90_106,
                    kind.clone(),
                    request.clone(),
                    vec![DurableOperationTransition::new(id(90_107)?, forbidden)],
                ),
                Err(RuntimeManifestError::InvalidLedger)
            );
        }
    }
    Ok(())
}

#[test]
fn locator_evidence_is_provider_or_compaction_only_and_latest_supersedes() -> TestResult {
    let operation = component(RuntimeComponentKind::Operation, b"operation-request")?;
    let prepared_locator = component(RuntimeComponentKind::Operation, b"prepared-locator")?;
    let learned_locator = component(RuntimeComponentKind::Operation, b"learned-locator")?;
    let terminal = component(RuntimeComponentKind::Operation, b"terminal-evidence")?;
    let record = DurableOperationRecord::new(
        91,
        DurableOperationKind::Compaction {
            workflow_id: workflow_id(0x91)?,
        },
        operation.clone(),
        vec![
            DurableOperationTransition::new(id(91_001)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(
                id(91_002)?,
                DurableOperationState::LocatorBound(prepared_locator.clone()),
            ),
            DurableOperationTransition::new(id(91_003)?, DurableOperationState::Dispatched),
            DurableOperationTransition::new(
                id(91_004)?,
                DurableOperationState::LocatorBound(learned_locator.clone()),
            ),
            DurableOperationTransition::new(
                id(91_005)?,
                DurableOperationState::Completed(terminal.clone()),
            ),
        ],
    )?;
    assert_eq!(record.latest_locator(), Some(&learned_locator));
    assert_eq!(record.transitions().len(), 5);

    let proven_not_dispatched = DurableOperationRecord::new(
        92,
        DurableOperationKind::Provider {
            root: RuntimeOperationRoot::Turn(id(92_001)?),
            attempt_id: id(92_002)?,
        },
        operation.clone(),
        vec![
            DurableOperationTransition::new(id(92_003)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(
                id(92_004)?,
                DurableOperationState::LocatorBound(prepared_locator.clone()),
            ),
            DurableOperationTransition::new(
                id(92_005)?,
                DurableOperationState::ProvenNotDispatched(terminal.clone()),
            ),
        ],
    )?;
    assert!(matches!(
        proven_not_dispatched.state(),
        DurableOperationState::ProvenNotDispatched(_)
    ));

    assert_eq!(
        DurableOperationRecord::new(
            93,
            DurableOperationKind::Effect {
                turn_id: id(93_001)?,
                attempt_id: id(93_002)?,
                call_id: id(93_003)?,
                source_index: 0,
            },
            operation.clone(),
            vec![
                DurableOperationTransition::new(id(93_004)?, DurableOperationState::Prepared),
                DurableOperationTransition::new(
                    id(93_005)?,
                    DurableOperationState::LocatorBound(learned_locator.clone()),
                ),
            ],
        ),
        Err(RuntimeManifestError::InvalidLedger)
    );
    assert_eq!(
        DurableOperationRecord::new(
            94,
            DurableOperationKind::Provider {
                root: RuntimeOperationRoot::Turn(id(94_001)?),
                attempt_id: id(94_002)?,
            },
            operation.clone(),
            vec![
                DurableOperationTransition::new(id(94_003)?, DurableOperationState::Prepared),
                DurableOperationTransition::new(
                    id(94_004)?,
                    DurableOperationState::LocatorBound(prepared_locator.clone()),
                ),
                DurableOperationTransition::new(
                    id(94_005)?,
                    DurableOperationState::LocatorBound(learned_locator.clone()),
                ),
            ],
        ),
        Err(RuntimeManifestError::InvalidLedger)
    );
    assert!(
        DurableOperationRecord::new(
            95,
            DurableOperationKind::Provider {
                root: RuntimeOperationRoot::Turn(id(95_001)?),
                attempt_id: id(95_002)?,
            },
            operation,
            vec![
                DurableOperationTransition::new(id(95_003)?, DurableOperationState::Prepared),
                DurableOperationTransition::new(id(95_004)?, DurableOperationState::Dispatched),
                DurableOperationTransition::new(
                    id(95_005)?,
                    DurableOperationState::LocatorBound(learned_locator),
                ),
                DurableOperationTransition::new(
                    id(95_006)?,
                    DurableOperationState::ProvenNotDispatched(terminal),
                ),
            ],
        )
        .is_ok()
    );
    assert!(
        DurableOperationRecord::new(
            96,
            DurableOperationKind::Provider {
                root: RuntimeOperationRoot::Turn(id(96_001)?),
                attempt_id: id(96_002)?,
            },
            component(RuntimeComponentKind::Operation, b"recovered-request")?,
            vec![
                DurableOperationTransition::new(id(96_003)?, DurableOperationState::Prepared),
                DurableOperationTransition::new(id(96_004)?, DurableOperationState::Dispatched),
                DurableOperationTransition::new(
                    id(96_005)?,
                    DurableOperationState::OutcomeUnknown(component(
                        RuntimeComponentKind::Operation,
                        b"ambiguous-provider-outcome",
                    )?),
                ),
                DurableOperationTransition::new(
                    id(96_006)?,
                    DurableOperationState::ProvenNotDispatched(component(
                        RuntimeComponentKind::Operation,
                        b"validated-provider-recovery",
                    )?),
                ),
            ],
        )
        .is_ok()
    );
    Ok(())
}

#[test]
fn lifecycle_rejects_mixed_operation_roots() -> TestResult {
    let lifecycle_turn = id(94_001)?;
    let other_turn = id(94_004)?;
    let operation = DurableOperationRecord::new(
        94,
        DurableOperationKind::Provider {
            root: RuntimeOperationRoot::Turn(other_turn),
            attempt_id: id(94_002)?,
        },
        component(RuntimeComponentKind::Operation, b"mixed-root-request")?,
        vec![DurableOperationTransition::new(
            id(94_003)?,
            DurableOperationState::Prepared,
        )],
    )?;
    let lifecycle = RuntimeLifecycle::Working {
        root: RuntimeWorkingRoot::Turn {
            turn_id: lifecycle_turn,
            next_round: 1,
        },
        recovery_phase: WorkingRecoveryPhase::DispatchEvidence,
        obligations: vec![RecoveryObligation::new(94, PROVIDER_HANDLER)],
    };
    let error = manifest(
        ProducerProvenance::new("plakous", "mixed-root", "offline")?,
        Some((
            lifecycle,
            RuntimeOperationLedger::new(Vec::new(), vec![operation])?,
        )),
    )
    .expect_err("mixed logical roots must fail closed");
    assert_eq!(
        error.downcast_ref::<RuntimeManifestError>(),
        Some(&RuntimeManifestError::InvalidLifecycle)
    );
    Ok(())
}

#[test]
fn turn_working_lifecycle_may_host_compaction_session_workflow() -> TestResult {
    let turn_id = id(95_001)?;
    let workflow_id = workflow_id(0x95)?;
    let request = |label: &'static [u8]| component(RuntimeComponentKind::Operation, label);
    let command = DurableOperationRecord::new(
        1,
        DurableOperationKind::Command {
            turn_id: Some(turn_id),
        },
        request(b"fence-b-command")?,
        vec![
            DurableOperationTransition::new(id(95_011)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(id(95_012)?, DurableOperationState::Dispatched),
        ],
    )?;
    let compaction = DurableOperationRecord::new(
        2,
        DurableOperationKind::Compaction { workflow_id },
        request(b"fence-b-compaction")?,
        vec![DurableOperationTransition::new(
            id(95_013)?,
            DurableOperationState::Prepared,
        )],
    )?;
    let provider = DurableOperationRecord::new(
        3,
        DurableOperationKind::Provider {
            root: RuntimeOperationRoot::SessionWorkflow(workflow_id),
            attempt_id: id(95_002)?,
        },
        request(b"fence-b-provider")?,
        vec![DurableOperationTransition::new(
            id(95_014)?,
            DurableOperationState::Prepared,
        )],
    )?;
    let lifecycle = RuntimeLifecycle::Working {
        root: RuntimeWorkingRoot::Turn {
            turn_id,
            next_round: 1,
        },
        recovery_phase: WorkingRecoveryPhase::DispatchEvidence,
        obligations: vec![
            RecoveryObligation::new(
                1,
                RecoveryHandlerSignature::new(
                    RecoveryObligationKind::Command,
                    RecoveryAction::FinalizationRequired,
                ),
            ),
            RecoveryObligation::new(
                2,
                RecoveryHandlerSignature::new(
                    RecoveryObligationKind::Compaction,
                    RecoveryAction::ProviderEvidenceRequired,
                ),
            ),
            RecoveryObligation::new(3, PROVIDER_HANDLER),
        ],
    };
    let recovered = manifest(
        ProducerProvenance::new("plakous", "fence-b-compaction-host", "offline")?,
        Some((
            lifecycle,
            RuntimeOperationLedger::new(Vec::new(), vec![command, compaction, provider])?,
        )),
    )?;
    assert!(matches!(
        recovered.lifecycle(),
        RuntimeLifecycle::Working {
            root: RuntimeWorkingRoot::Turn {
                turn_id: observed,
                ..
            },
            recovery_phase: WorkingRecoveryPhase::DispatchEvidence,
            ..
        } if *observed == turn_id
    ));
    Ok(())
}

#[test]
fn command_append_rejects_duplicate_identity_through_persistent_index() -> TestResult {
    let ledger = RuntimeOperationLedger::new(Vec::new(), Vec::new())?
        .append_command(command_record("same-command", 1)?)?;
    assert_eq!(
        ledger.append_command(command_record("same-command", 2)?),
        Err(RuntimeManifestError::InvalidLedger)
    );
    assert_eq!(ledger.commands().len(), 1);
    Ok(())
}

#[test]
fn effect_reconciliation_accepts_mixed_ambiguous_and_finalization_obligations() -> TestResult {
    let turn_id = id(70_001)?;
    let attempt_id = id(70_002)?;
    let completed = DurableOperationRecord::new(
        1,
        DurableOperationKind::Effect {
            turn_id,
            attempt_id,
            call_id: id(70_003)?,
            source_index: 0,
        },
        component(RuntimeComponentKind::Operation, b"completed-effect-request")?,
        vec![
            DurableOperationTransition::new(id(70_004)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(id(70_005)?, DurableOperationState::Dispatched),
            DurableOperationTransition::new(
                id(70_006)?,
                DurableOperationState::Completed(component(
                    RuntimeComponentKind::Operation,
                    b"completed-effect-outcome",
                )?),
            ),
        ],
    )?;
    let ambiguous = DurableOperationRecord::new(
        2,
        DurableOperationKind::Effect {
            turn_id,
            attempt_id,
            call_id: id(70_007)?,
            source_index: 1,
        },
        component(RuntimeComponentKind::Operation, b"ambiguous-effect-request")?,
        vec![
            DurableOperationTransition::new(id(70_008)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(id(70_009)?, DurableOperationState::Dispatched),
            DurableOperationTransition::new(
                id(70_010)?,
                DurableOperationState::OutcomeUnknown(component(
                    RuntimeComponentKind::Operation,
                    b"ambiguous-effect-evidence",
                )?),
            ),
        ],
    )?;
    let lifecycle = RuntimeLifecycle::Working {
        root: RuntimeWorkingRoot::Turn {
            turn_id,
            next_round: 2,
        },
        recovery_phase: WorkingRecoveryPhase::EffectReconciliation,
        obligations: vec![
            RecoveryObligation::new(
                1,
                RecoveryHandlerSignature::new(
                    RecoveryObligationKind::Effect,
                    RecoveryAction::FinalizationRequired,
                ),
            ),
            RecoveryObligation::new(
                2,
                RecoveryHandlerSignature::new(
                    RecoveryObligationKind::Effect,
                    RecoveryAction::EffectEvidenceRequired,
                ),
            ),
        ],
    };
    let ledger = RuntimeOperationLedger::new(Vec::new(), vec![completed, ambiguous])?;
    let manifest = manifest(
        ProducerProvenance::new("plakous", "mixed-effect-build", "offline")?,
        Some((lifecycle, ledger)),
    )?;
    assert_eq!(
        manifest.lifecycle().checkpoint_kind(),
        RuntimeCheckpointKind::Working
    );
    Ok(())
}

#[test]
fn checkpoint_construction_and_graph_publication_are_compositional() -> TestResult {
    let operation = DurableOperationRecord::new(
        1,
        DurableOperationKind::Command { turn_id: None },
        component(RuntimeComponentKind::Operation, b"checkpoint-operation-v1")?,
        vec![
            DurableOperationTransition::new(id(60_001)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(
                id(60_002)?,
                DurableOperationState::ProvenNotDispatched(component(
                    RuntimeComponentKind::Operation,
                    b"checkpoint-nondispatch-v1",
                )?),
            ),
            DurableOperationTransition::new(
                id(60_003)?,
                DurableOperationState::Finalized(component(
                    RuntimeComponentKind::Operation,
                    b"checkpoint-nondispatch-v1",
                )?),
            ),
        ],
    )?;
    let ledger = RuntimeOperationLedger::new(
        vec![command_record("checkpoint-command", 1)?],
        vec![operation],
    )?;
    let manifest = manifest(
        ProducerProvenance::new("plakous", "composition-build", "offline")?,
        Some((RuntimeLifecycle::Quiescent, ledger)),
    )?;
    let checkpoint = RuntimeCheckpoint::new(manifest.clone())?;
    assert_eq!(
        RuntimeCheckpoint::new(manifest)?.integrity(),
        checkpoint.integrity()
    );

    let bundle = crate::RuntimeCheckpointCodec::bundle(&checkpoint, None)?;

    let next_operation = DurableOperationRecord::new(
        2,
        DurableOperationKind::Command { turn_id: None },
        component(
            RuntimeComponentKind::Operation,
            b"next-checkpoint-operation-v1",
        )?,
        vec![
            DurableOperationTransition::new(id(61_001)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(
                id(61_002)?,
                DurableOperationState::ProvenNotDispatched(component(
                    RuntimeComponentKind::Operation,
                    b"next-checkpoint-nondispatch-v1",
                )?),
            ),
            DurableOperationTransition::new(
                id(61_003)?,
                DurableOperationState::Finalized(component(
                    RuntimeComponentKind::Operation,
                    b"next-checkpoint-nondispatch-v1",
                )?),
            ),
        ],
    )?;
    let prior = checkpoint.manifest();
    let next_ledger = prior
        .ledger
        .append_command(command_record("next-checkpoint-command", 2)?)?
        .append_operation(next_operation)?;
    let mut next_topology = prior.topology;
    next_topology.parent_checkpoint_id = Some(next_topology.checkpoint_id);
    next_topology.checkpoint_id = id(61_004)?;
    next_topology.branch_revision += 1;
    next_topology.commit_identity = digest(DigestDomain::ProviderOperation, b"next-commit")?;
    let next_checkpoint = RuntimeCheckpoint::new(RuntimeManifest::new(
        prior.provenance.clone(),
        next_topology,
        RuntimeLifecycle::Quiescent,
        prior.harness_plan.clone(),
        prior.provider_configuration.clone(),
        prior.canonical.clone(),
        prior.binding.clone(),
        prior.tools.clone(),
        next_ledger,
        prior.family_artifact.clone(),
        prior.interpreter_observation.clone(),
        prior.jobs.clone(),
        prior.durability,
    )?)?;
    let advanced = crate::RuntimeCheckpointCodec::advance(&next_checkpoint, &bundle)?;
    assert_eq!(
        advanced.validated_checkpoint()?.integrity(),
        next_checkpoint.integrity()
    );
    assert_eq!(
        crate::RuntimeCheckpointCodec::materialize(&advanced)?,
        next_checkpoint
    );
    Ok(())
}

#[test]
fn effect_decision_pause_is_exact_pre_dispatch_and_closed() -> TestResult {
    let request = component(
        RuntimeComponentKind::Operation,
        b"pending-effect-decision-v1",
    )?;
    let pending = PendingEffectDecisionState::new(request.clone())?;
    assert!(!pending.dispatch_capability_minted());
    assert_eq!(pending.request(), &request);
    let kind = DurableOperationKind::EffectDecision {
        turn_id: id(70_001)?,
        attempt_id: id(70_002)?,
        call_id: id(70_003)?,
    };
    let record = DurableOperationRecord::new(
        1,
        kind.clone(),
        request.clone(),
        vec![
            DurableOperationTransition::new(id(70_004)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(
                id(70_005)?,
                DurableOperationState::DecisionRequired(pending),
            ),
        ],
    )?;
    assert!(matches!(
        record.state(),
        DurableOperationState::DecisionRequired(state)
            if !state.dispatch_capability_minted() && state.request() == record.request()
    ));
    assert_eq!(
        DurableOperationRecord::new(
            2,
            DurableOperationKind::Effect {
                turn_id: id(70_001)?,
                attempt_id: id(70_002)?,
                call_id: id(70_003)?,
                source_index: 0,
            },
            request.clone(),
            vec![
                DurableOperationTransition::new(id(70_006)?, DurableOperationState::Prepared,),
                DurableOperationTransition::new(
                    id(70_007)?,
                    DurableOperationState::DecisionRequired(PendingEffectDecisionState::new(
                        request.clone()
                    )?,),
                ),
            ],
        ),
        Err(RuntimeManifestError::InvalidLedger)
    );
    assert_eq!(
        record.append_transition(DurableOperationTransition::new(
            id(70_008)?,
            DurableOperationState::Dispatched,
        )),
        Err(RuntimeManifestError::InvalidLedger)
    );
    let completed = record.append_transition(DurableOperationTransition::new(
        id(70_009)?,
        DurableOperationState::ProvenNotDispatched(component(
            RuntimeComponentKind::Operation,
            b"decision-cancelled-v1",
        )?),
    ))?;
    assert!(matches!(
        completed.state(),
        DurableOperationState::ProvenNotDispatched(_)
    ));
    Ok(())
}

#[test]
fn lifecycle_codec_golden_matrix_is_closed_bounded_and_tamper_checked() -> TestResult {
    assert_eq!(RUNTIME_FORMAT_EPOCH, 27);
    assert_eq!(RUNTIME_LIFECYCLE_CODEC_REVISION, 3);
    let signature = RecoveryHandlerSignature::new;
    let command = RecoveryObligation::new(
        11,
        signature(
            RecoveryObligationKind::Command,
            RecoveryAction::ProvenNotDispatched,
        ),
    );
    let provider = RecoveryObligation::new(
        12,
        signature(
            RecoveryObligationKind::ProviderAttempt,
            RecoveryAction::ProviderEvidenceRequired,
        ),
    );
    let decision = RecoveryObligation::new(
        13,
        signature(
            RecoveryObligationKind::EffectDecision,
            RecoveryAction::FinalizationRequired,
        ),
    );
    let effect = RecoveryObligation::new(
        14,
        signature(
            RecoveryObligationKind::Effect,
            RecoveryAction::EffectEvidenceRequired,
        ),
    );
    let commit = RecoveryObligation::new(
        15,
        signature(
            RecoveryObligationKind::Commit,
            RecoveryAction::CommitRecovery,
        ),
    );
    let finalization = RecoveryObligation::new(
        16,
        signature(
            RecoveryObligationKind::ProviderAttempt,
            RecoveryAction::FinalizationRequired,
        ),
    );
    let compaction = RecoveryObligation::new(
        17,
        signature(
            RecoveryObligationKind::Compaction,
            RecoveryAction::ProviderEvidenceRequired,
        ),
    );
    let cases = [
        (
            "quiescent",
            RuntimeLifecycle::Quiescent,
            55,
            "3f2ec9246e958af728dfd194148edc2a0a531d1dcc9168d909f8db218d4c7d9c",
        ),
        (
            "dispatch-evidence",
            RuntimeLifecycle::Working {
                root: RuntimeWorkingRoot::Turn {
                    turn_id: id(0x100)?,
                    next_round: 1,
                },
                recovery_phase: WorkingRecoveryPhase::DispatchEvidence,
                obligations: vec![command, provider],
            },
            105,
            "ff52f6a1a9fc69fd2dabae3f3033e50e3422f0c9e540ae7990c7be58f6b08f2b",
        ),
        (
            "effect-reconciliation",
            RuntimeLifecycle::Working {
                root: RuntimeWorkingRoot::Turn {
                    turn_id: id(0x200)?,
                    next_round: 2,
                },
                recovery_phase: WorkingRecoveryPhase::EffectReconciliation,
                obligations: vec![decision, effect],
            },
            105,
            "885f2d6b626ab9c148305a6c14e45fecf5215780ee52babf4db405f37681cfb9",
        ),
        (
            "commit-recovery",
            RuntimeLifecycle::Working {
                root: RuntimeWorkingRoot::Turn {
                    turn_id: id(0x300)?,
                    next_round: 3,
                },
                recovery_phase: WorkingRecoveryPhase::CommitRecovery,
                obligations: vec![commit],
            },
            95,
            "8aa3d6ea300ced8ba2d4ce650569244ee8431beca83d2f400e2d1776690c571d",
        ),
        (
            "finalization",
            RuntimeLifecycle::Working {
                root: RuntimeWorkingRoot::Turn {
                    turn_id: id(0x400)?,
                    next_round: 4,
                },
                recovery_phase: WorkingRecoveryPhase::Finalization,
                obligations: vec![finalization],
            },
            95,
            "c16e42eef9108fb19f05c5b133995e7486faf7b8b4d8f8b0b2f94667db057bde",
        ),
        (
            "session-workflow",
            RuntimeLifecycle::Working {
                root: RuntimeWorkingRoot::SessionWorkflow(workflow_id(0x51)?),
                recovery_phase: WorkingRecoveryPhase::DispatchEvidence,
                obligations: vec![compaction],
            },
            107,
            "fd03c57bffa7f41f65b86a6b486d4dceaa15c37e16a8175b5d5523923751d589",
        ),
    ];

    for (label, lifecycle, expected_length, expected_digest) in &cases {
        let encoded = RuntimeLifecycleCodec::encode(lifecycle)?;
        assert_eq!(encoded.len(), *expected_length, "{label}");
        assert_eq!(
            hex(Digest::sha256(DigestDomain::Blob, 1, &[&encoded])?.bytes()),
            *expected_digest,
            "{label}"
        );
        assert_eq!(RuntimeLifecycleCodec::decode(&encoded)?, lifecycle.clone());
    }

    let valid = RuntimeLifecycleCodec::encode(&cases[1].1)?;
    let mut tampered = valid.clone();
    tampered[16] ^= 1;
    assert_eq!(
        RuntimeLifecycleCodec::decode(&tampered),
        Err(RuntimeManifestError::IntegrityMismatch)
    );
    let mut trailing = valid.clone();
    trailing.push(0);
    assert_eq!(
        RuntimeLifecycleCodec::decode(&trailing),
        Err(RuntimeManifestError::TrailingBytes)
    );
    let body = decode_envelope(
        &valid,
        LIFECYCLE_MAGIC,
        RUNTIME_LIFECYCLE_CODEC_REVISION,
        MAX_RUNTIME_LIFECYCLE_BYTES,
        b"runtime-lifecycle",
    )?;
    let rewrap = |body: &[u8]| {
        encode_envelope(
            LIFECYCLE_MAGIC,
            RUNTIME_LIFECYCLE_CODEC_REVISION,
            body,
            root_integrity(b"runtime-lifecycle", body)?,
            MAX_RUNTIME_LIFECYCLE_BYTES,
        )
    };
    for (offset, field) in [
        (0, "lifecycle"),
        (1, "working operation root"),
        (22, "working recovery phase"),
        (39, "recovery obligation"),
        (40, "recovery action"),
    ] {
        let mut unknown = body.to_vec();
        unknown[offset] = u8::MAX;
        assert_eq!(
            RuntimeLifecycleCodec::decode(&rewrap(&unknown)?),
            Err(RuntimeManifestError::UnknownTag {
                field,
                tag: u8::MAX,
            })
        );
    }
    let mut zero_operation = body.to_vec();
    zero_operation[31..39].fill(0);
    assert_eq!(
        RuntimeLifecycleCodec::decode(&rewrap(&zero_operation)?),
        Err(RuntimeManifestError::InvalidLifecycle)
    );
    let workflow = RuntimeLifecycleCodec::encode(&cases[5].1)?;
    let workflow_body = decode_envelope(
        &workflow,
        LIFECYCLE_MAGIC,
        RUNTIME_LIFECYCLE_CODEC_REVISION,
        MAX_RUNTIME_LIFECYCLE_BYTES,
        b"runtime-lifecycle",
    )?;
    let mut zero_workflow = workflow_body.to_vec();
    zero_workflow[2..34].fill(0);
    assert_eq!(
        RuntimeLifecycleCodec::decode(&rewrap(&zero_workflow)?),
        Err(RuntimeManifestError::Malformed)
    );
    assert_eq!(
        RuntimeLifecycleCodec::decode(&vec![0; MAX_RUNTIME_LIFECYCLE_BYTES + 1]),
        Err(RuntimeManifestError::EnvelopeSize)
    );
    let over_bound_count = u64::try_from(MAX_RUNTIME_RECORDS)?.saturating_add(1);
    let over_bound = RuntimeLifecycle::Working {
        root: RuntimeWorkingRoot::Turn {
            turn_id: id(0x500)?,
            next_round: 1,
        },
        recovery_phase: WorkingRecoveryPhase::Finalization,
        obligations: (1..=over_bound_count)
            .map(|operation_id| RecoveryObligation::new(operation_id, finalization.signature()))
            .collect(),
    };
    assert_eq!(
        RuntimeLifecycleCodec::encode(&over_bound),
        Err(RuntimeManifestError::InvalidLifecycle)
    );
    Ok(())
}

#[test]
fn hosted_lifecycle_tags_are_explicit_closed_goldens() -> TestResult {
    let turn_id = id(0x611)?;
    let attempt_id = id(0x612)?;
    for (kind, expected) in [
        (
            DurableOperationKind::ToolBatchContinuation {
                turn_id,
                attempt_id,
            },
            11,
        ),
        (
            DurableOperationKind::ServerPauseContinuation {
                turn_id,
                attempt_id,
            },
            12,
        ),
    ] {
        let mut writer = Writer { bytes: Vec::new() };
        writer.operation_kind(&kind)?;
        assert_eq!(writer.bytes[0], expected);
        let mut reader = Reader::new(&writer.bytes);
        assert_eq!(reader.operation_kind()?, kind);
        reader.finish()?;
    }

    assert_eq!(
        obligation_kind_tag(RecoveryObligationKind::HostedContinuation),
        9
    );
    assert_eq!(
        obligation_kind(9)?,
        RecoveryObligationKind::HostedContinuation
    );
    assert_eq!(recovery_action_tag(RecoveryAction::RestoreToolBatch), 8);
    assert_eq!(recovery_action(8)?, RecoveryAction::RestoreToolBatch);
    assert_eq!(recovery_action_tag(RecoveryAction::RestoreServerPause), 9);
    assert_eq!(recovery_action(9)?, RecoveryAction::RestoreServerPause);
    assert_eq!(
        recovery_phase_tag(WorkingRecoveryPhase::HostedContinuation),
        6
    );
    assert_eq!(recovery_phase(6)?, WorkingRecoveryPhase::HostedContinuation);

    for (actual, field) in [
        (
            obligation_kind(u8::MAX).expect_err("unknown obligation tag"),
            "recovery obligation",
        ),
        (
            recovery_action(u8::MAX).expect_err("unknown action tag"),
            "recovery action",
        ),
        (
            recovery_phase(u8::MAX).expect_err("unknown phase tag"),
            "working recovery phase",
        ),
    ] {
        assert_eq!(
            actual,
            RuntimeManifestError::UnknownTag {
                field,
                tag: u8::MAX,
            }
        );
    }
    let mut reader = Reader::new(&[u8::MAX]);
    assert_eq!(
        reader.operation_kind(),
        Err(RuntimeManifestError::UnknownTag {
            field: "operation kind",
            tag: u8::MAX,
        })
    );
    Ok(())
}

#[test]
fn checkpoint_graph_golden_and_exact_artifact_bytes() -> TestResult {
    let checkpoint = quiescent("golden-build")?;
    let bundle = crate::RuntimeCheckpointCodec::bundle(&checkpoint, None)?;
    assert_eq!(bundle.root().codec_revision(), RUNTIME_FORMAT_EPOCH);
    assert!(
        bundle
            .objects()
            .iter()
            .all(|object| { object.address().codec_revision() == RUNTIME_FORMAT_EPOCH })
    );
    let decoded = crate::RuntimeCheckpointCodec::materialize(&bundle)?;
    assert_eq!(decoded, checkpoint);
    assert_eq!(
        decoded
            .manifest()
            .family_artifact()
            .expect("fixture artifact")
            .bytes(),
        b"exact-responses-artifact-v1"
    );
    Ok(())
}

#[test]
fn family_artifact_is_exact_redacted_responses_v3_state() -> TestResult {
    const SECRET: &[u8] = b"FAMILY_ARTIFACT_SECRET_SENTINEL";
    let binding = digest(DigestDomain::ModelBinding, b"family-binding")?;
    let artifact = FamilyArtifact::new(
        "responses",
        RESPONSES_FAMILY_ARTIFACT_SCHEMA_REVISION,
        RESPONSES_FAMILY_ARTIFACT_CODEC_REVISION,
        binding,
        SECRET,
    )?;
    let active = artifact.to_active()?;
    assert_eq!(active.family(), "responses");
    assert_eq!(
        active.schema_revision(),
        RESPONSES_FAMILY_ARTIFACT_SCHEMA_REVISION
    );
    assert_eq!(
        active.codec_revision(),
        RESPONSES_FAMILY_ARTIFACT_CODEC_REVISION
    );
    assert_eq!(active.binding(), binding);
    assert_eq!(active.bytes(), SECRET);
    assert_eq!(
        family_binding_digest(artifact.binding_component())?,
        binding
    );
    assert!(!format!("{artifact:?}").contains("FAMILY_ARTIFACT_SECRET_SENTINEL"));
    assert_eq!(
        FamilyArtifact::new(
            "messages",
            RESPONSES_FAMILY_ARTIFACT_SCHEMA_REVISION,
            RESPONSES_FAMILY_ARTIFACT_CODEC_REVISION,
            binding,
            SECRET,
        ),
        Err(RuntimeManifestError::RevisionDisagreement)
    );
    assert_eq!(
        FamilyArtifact::new(
            "responses",
            RESPONSES_FAMILY_ARTIFACT_SCHEMA_REVISION.saturating_add(1),
            RESPONSES_FAMILY_ARTIFACT_CODEC_REVISION,
            binding,
            SECRET,
        ),
        Err(RuntimeManifestError::RevisionDisagreement)
    );
    assert_eq!(
        FamilyArtifact::new(
            "responses",
            RESPONSES_FAMILY_ARTIFACT_SCHEMA_REVISION,
            RESPONSES_FAMILY_ARTIFACT_CODEC_REVISION,
            digest(DigestDomain::Blob, b"wrong-binding-domain")?,
            SECRET,
        ),
        Err(RuntimeManifestError::InvalidDigest)
    );
    Ok(())
}

#[test]
fn oversized_reconstructive_component_fails_closed() {
    assert_eq!(
        ReconstructiveComponent::new(
            RuntimeComponentKind::ContextState,
            1,
            vec![0; MAX_RUNTIME_COMPONENT_BYTES + 1],
        ),
        Err(RuntimeManifestError::ComponentSize)
    );
}

#[test]
fn reconstructive_component_bounds_are_per_kind_and_preserve_the_codec() -> TestResult {
    const OPERATION_LIMIT: usize = 64 * 1024 * 1024;
    assert_eq!(MAX_RUNTIME_COMPONENT_BYTES, 4 * 1024 * 1024);
    for tag in 1..=12 {
        let kind = RuntimeComponentKind::from_tag(tag)?;
        let limit = if kind == RuntimeComponentKind::Operation {
            OPERATION_LIMIT
        } else {
            MAX_RUNTIME_COMPONENT_BYTES
        };
        for length in [
            0,
            MAX_RUNTIME_COMPONENT_BYTES,
            MAX_RUNTIME_COMPONENT_BYTES + 1,
            limit,
            limit + 1,
        ] {
            let bytes = vec![0x5a; length];
            let integrity = component_integrity(kind, 1, &bytes)?;
            // Construct the exact existing wire shape independently of the writer,
            // including rejected payloads, so a reader-only bound regression fails.
            let mut wire = vec![tag];
            wire.extend_from_slice(&1_u32.to_be_bytes());
            wire.extend_from_slice(&u32::try_from(length)?.to_be_bytes());
            wire.extend_from_slice(&bytes);
            let mut digest_writer = Writer::default();
            digest_writer.digest(integrity)?;
            wire.extend_from_slice(&digest_writer.bytes);
            if length == 0 || length > limit {
                assert_eq!(
                    ReconstructiveComponent::new(kind, 1, bytes),
                    Err(RuntimeManifestError::ComponentSize),
                    "{kind:?}/{length}"
                );
                assert_eq!(
                    decode_bundle_component(&wire),
                    Err(RuntimeManifestError::ComponentSize),
                    "{kind:?}/{length}"
                );
                // A forged internal value must not let the writer bypass its bound.
                let forged = ReconstructiveComponent {
                    kind,
                    codec_revision: 1,
                    bytes: vec![0x5a; length].into(),
                    integrity,
                };
                if length > limit {
                    assert_eq!(
                        encode_bundle_component(&forged),
                        Err(RuntimeManifestError::ComponentSize)
                    );
                }
            } else {
                let component = ReconstructiveComponent::new(kind, 1, bytes)?;
                assert_eq!(encode_bundle_component(&component)?, wire);
                assert_eq!(decode_bundle_component(&wire)?, component);
            }
        }
    }
    Ok(())
}

#[test]
fn build_provenance_is_not_reopen_authority() -> TestResult {
    let left = quiescent("build-a")?;
    let right = quiescent("build-b")?;
    assert_ne!(
        crate::RuntimeCheckpointCodec::bundle(&left, None)?.root(),
        crate::RuntimeCheckpointCodec::bundle(&right, None)?.root()
    );
    assert!(matches!(
        left.rehydrate(&witness(left.manifest())?)?,
        RuntimeRehydration::Quiescent { .. }
    ));
    assert!(matches!(
        right.rehydrate(&witness(right.manifest())?)?,
        RuntimeRehydration::Quiescent { .. }
    ));
    Ok(())
}

#[test]
fn rehydration_reconstructs_plan_and_refuses_an_unrecoverable_working_scope() -> TestResult {
    let checkpoint = quiescent("rehydrate-build")?;
    let revalidation = witness(checkpoint.manifest())?;
    let RuntimeRehydration::Quiescent {
        checkpoint: rebuilt,
        harness_plan,
    } = checkpoint.rehydrate(&revalidation)?
    else {
        return Err("quiescent checkpoint reopened as working".into());
    };
    assert_eq!(rebuilt, checkpoint);
    assert_eq!(
        checkpoint
            .manifest()
            .canonical()
            .stable_prefix()
            .resolved_harness_plan_identity(),
        Some(*harness_plan.plan_digest())
    );

    // A lone provider-evidence obligation without its root command is not a
    // shape this build recovers.
    let working = working()?;
    let revalidation = witness(working.manifest())?;
    assert!(matches!(
        working.rehydrate(&revalidation),
        Err(crate::RuntimeRehydrationError::UnsupportedRecoveryScope(_))
    ));
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes.iter().fold(String::new(), |mut output, byte| {
        let _ = write!(output, "{byte:02x}");
        output
    })
}

/// A durable record's absent-field shape is frozen, in both directions.
///
/// Every site listed here decodes by re-encoding what it read and demanding
/// the stored bytes back byte for byte. That makes the *shape* of an absent
/// optional field part of the durable format, not a serde detail:
///
/// - A field that has always emitted `null` must keep emitting `null`. Adding
///   `skip_serializing_if` to one would omit it on re-encode, the round trip
///   would fail against every record already on disk, and every session
///   written by an earlier build would stop reopening.
/// - A field added later must be omitted when absent, so a record written
///   before it existed re-encodes to exactly the bytes it was stored as. This
///   is the direction that broke on 2026-09-18: `TurnUsage::peak_input_tokens`
///   decoded from nothing and re-encoded as `null`, and a candidate 2 session
///   could not be reopened by candidate 5.
///
/// Neither rule is deducible from the type, so it is pinned here.
#[test]
fn a_round_tripped_records_absent_field_shape_is_frozen() -> TestResult {
    // Present since the format existed: `null`, and it must stay `null`.
    let request = CompositionEvaluationRequest {
        version: 2,
        descriptor_identity: Digest::sha256(DigestDomain::ToolDefinition, 1, &[b"descriptor"])?,
        host_identity: b"host".to_vec(),
        call: ToolCallBlock::new(
            CallId::try_from(Uuid::from_u128(77_001))?,
            ToolName::new("probe")?,
            ToolExecutionOwner::Local,
            ToolCallState::Requested,
            ToolCorrelation::default(),
            "{}",
        )?,
        arguments_digest: Digest::sha256(DigestDomain::ToolDefinition, 1, &[b"arguments"])?,
        source_digest: None,
        result_entry_id: EntryId::try_from(Uuid::from_u128(77_002))?,
    };
    let value = serde_json::to_value(&request)?;
    assert_eq!(
        value.get("source_digest"),
        Some(&serde_json::Value::Null),
        "an absent field that has always been written as null must stay null, \
         or every record already on disk stops round tripping"
    );
    // And the record the encoder produces is readable by the decoder that
    // enforces the round trip.
    let bytes = serde_json::to_vec(&("plakous-local-composition-evaluation-v2", &request))?;
    decode_exact_composition_request::<CompositionEvaluationRequest>(
        &bytes,
        "plakous-local-composition-evaluation-v2",
    )?;

    // Added later: omitted when absent, so older bytes re-encode unchanged.
    let usage = crate::TurnUsage::default();
    assert!(
        serde_json::to_value(usage)?
            .get("peak_input_tokens")
            .is_none(),
        "a field added after records existed must be omitted when absent"
    );
    Ok(())
}

#[test]
fn removed_runtime_context_transition_tags_are_refused() {
    for tag in [3, 4] {
        assert_eq!(
            RuntimeContextTransitionKind::from_tag(tag),
            Err(RuntimeManifestError::UnknownTag {
                field: "runtime context transition",
                tag
            })
        );
    }
}

fn job_row(number: u32) -> TestResult<JobRow> {
    Ok(JobRow {
        number,
        command: "sleep 60".to_owned(),
        started_at: Timestamp::new(10, 0)?,
        owner: "agent-1".to_owned(),
        session_id: ProcessSessionId::new([7; 16])?,
        owner_generation: ProcessSessionOwnerGeneration::new(1)?,
        terminal: None,
    })
}

#[test]
fn quiescent_job_ledger_and_terminal_evidence_round_trip() -> TestResult {
    assert_eq!(RUNTIME_FORMAT_EPOCH, 27);
    let mut manifest = manifest(ProducerProvenance::new("plakous", "jobs", "offline")?, None)?;
    let mut row = job_row(3)?;
    row.terminal = Some(JobTerminal {
        status: JobTerminalStatus::Stopped,
        exit_code: None,
        stopper: Some(plakous_tools::JobStopReason::SessionExit),
        elapsed: std::time::Duration::new(12, 34),
        tail: "tail\n".to_owned(),
    });
    manifest.jobs = JobLedger::new(4, vec![row])?;
    let checkpoint = RuntimeCheckpoint::new(manifest)?;
    assert_eq!(checkpoint.kind(), RuntimeCheckpointKind::Quiescent);
    let bundle = crate::RuntimeCheckpointCodec::bundle(&checkpoint, None)?;
    let decoded = crate::RuntimeCheckpointCodec::materialize(&bundle)?;
    assert_eq!(decoded.manifest().jobs(), checkpoint.manifest().jobs());
    assert_eq!(decoded.integrity(), checkpoint.integrity());
    assert_eq!(
        decoded.manifest().jobs().cleared(),
        JobLedger::new(4, Vec::new())?
    );
    Ok(())
}

#[test]
fn job_ledger_bounds_numbers_and_same_batch_consumption() -> TestResult {
    assert!(JobLedger::new(0, Vec::new()).is_err());
    assert!(JobLedger::new(1, vec![job_row(1)?]).is_err());
    assert!(JobLedger::new(3, vec![job_row(1)?, job_row(1)?]).is_err());
    let rows = (1..=128).map(job_row).collect::<TestResult<Vec<_>>>()?;
    let full = JobLedger::new(129, rows)?;
    assert!(full.updated(130, [job_row(129)?], &[]).is_err());
    let consumed = full.updated(130, [job_row(129)?], &[1, 129])?;
    assert_eq!(consumed.rows().len(), 127);
    assert_eq!(consumed.next_id(), 130);
    let mut bad = job_row(1)?;
    bad.command = "x".repeat(1025);
    assert!(JobLedger::new(2, vec![bad]).is_err());
    let mut bad = job_row(1)?;
    bad.terminal = Some(JobTerminal {
        status: JobTerminalStatus::Exited,
        exit_code: Some(0),
        stopper: None,
        elapsed: std::time::Duration::ZERO,
        tail: "x".repeat(4097),
    });
    assert!(JobLedger::new(2, vec![bad]).is_err());
    Ok(())
}

#[test]
fn replay_omission_strategy_codec_tag_is_four_and_round_trips() -> TestResult {
    let source = RuntimeContextState::genesis(id(83_001)?, id(83_002)?, [0x63; 32])?;
    let (compacted, _, _) =
        compact_context(&source, &context_history(4)?, id(83_003)?, [0x64; 32])?;
    let mut provenance = compacted
        .compaction_provenance()
        .ok_or("provenance")?
        .clone();
    provenance.strategy = RuntimeCompactionStrategy::ReplayOmissionV1;
    provenance.selected_group_count = 1;
    provenance.identity = compaction_provenance_identity(&provenance)?;
    assert_eq!(provenance.strategy.tag(), 4);
    let mut writer = Writer::default();
    write_compaction_provenance_fields(&mut writer, &provenance)?;
    writer.digest(provenance.identity)?;
    let mut tag_reader = Reader::new(&writer.bytes);
    tag_reader.checkpoint_id()?;
    for _ in 0..6 {
        tag_reader.digest(None)?;
    }
    assert_eq!(tag_reader.u8()?, 4);
    let mut reader = Reader::new(&writer.bytes);
    assert_eq!(read_compaction_provenance(&mut reader)?, provenance);
    reader.finish()?;
    Ok(())
}

#[test]
fn replay_omission_provenance_rejects_nonunit_groups() -> TestResult {
    let source = RuntimeContextState::genesis(id(84_001)?, id(84_002)?, [0x63; 32])?;
    let (compacted, _, _) =
        compact_context(&source, &context_history(4)?, id(84_003)?, [0x64; 32])?;
    let mut provenance = compacted
        .compaction_provenance()
        .ok_or("provenance")?
        .clone();
    provenance.strategy = RuntimeCompactionStrategy::ReplayOmissionV1;
    for groups in [0, 2] {
        provenance.selected_group_count = groups;
        provenance.identity = compaction_provenance_identity(&provenance)?;
        let mut writer = Writer::default();
        write_compaction_provenance_fields(&mut writer, &provenance)?;
        writer.digest(provenance.identity)?;
        assert!(read_compaction_provenance(&mut Reader::new(&writer.bytes)).is_err());
    }
    provenance.strategy = RuntimeCompactionStrategy::ToolResultPlaceholdersV1;
    provenance.selected_group_count = 0;
    assert!(provenance.validate().is_err());
    Ok(())
}
