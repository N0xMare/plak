use super::*;

/// Whether a V2 projection is the qualified candidate is the composition
/// root's fact, so the agent holds no default answer: a builder given no port
/// carries none, and `construction_tool_presentation` refuses a
/// standard-integration projection when it finds none. Installing a port
/// stores exactly that authority and does not consult it early.
#[test]
fn projection_candidate_admission_is_absent_until_the_composition_root_supplies_it() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let builder = artifact_test_builder(workspace, artifact_test_model(1, 7_500)?);
    assert!(builder.projection_admission.is_none());

    for admits in [true, false] {
        let admission = FakeProjectionAdmission::new(admits);
        let port: Arc<dyn ProjectionCandidateAdmission> = Arc::clone(&admission) as _;
        let builder = artifact_test_builder(
            Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
            artifact_test_model(1, 7_500)?,
        )
        .projection_admission(Arc::clone(&port));
        let installed = builder
            .projection_admission
            .as_ref()
            .ok_or("installed admission is absent")?;
        assert!(Arc::ptr_eq(installed, &port));
        // The agent asks at construction, not at installation.
        assert_eq!(admission.asked.load(Ordering::SeqCst), 0);
    }
    Ok(())
}

#[test]
fn agent_builder_default_policy_remains_read_only() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let builder = artifact_test_builder(workspace, artifact_test_model(1, 7_500)?);

    let revision = builder.tool_policy_revision();
    assert_eq!(revision.policy_id(), "plakous.read-only");
    assert_eq!(revision.revision(), 1);
    Ok(())
}

#[test]
fn harness_compaction_prefix_preserves_exact_processing_tier() -> TestResult {
    let registry = one_mutating_tool_registry()?;
    let configured =
        configured_native_compact_model("test.route", Timestamp::new(1, 0)?, 1, &registry)?;
    let selected = configured.configuration().selected_identity();
    let effective = configured.configuration().effective_identity();
    let priority = plakous_model::ProcessingTierDisposition::ProviderNamed(
        plakous_model::ProviderServiceTierId::new("priority")?,
    );
    let behavior = configured.configuration().effective().behavior();
    let source = StableRequestPrefix::new(
        selected,
        effective,
        priority.clone(),
        Vec::new(),
        behavior.tools.clone(),
        Vec::new(),
        OutputPolicy::default(),
        ReasoningPolicy::default(),
        ToolChoice::Auto,
        behavior.cache,
        GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
        NativeOptions::default(),
        None,
    )?
    .with_context_admission_requirement(ContextAdmissionRequirement::AnyBounded)?;

    let prefix = harness_compaction_prefix(&source, selected, effective)?;

    assert_eq!(prefix.processing_tier(), &priority);
    assert_eq!(prefix.selected_provider_configuration_id(), selected);
    assert_eq!(prefix.effective_provider_configuration_id(), effective);
    assert_eq!(prefix.tool_choice(), &ToolChoice::None);
    // The summary bound never exceeds the turn's own 128-token bound.
    assert_eq!(prefix.limits().max_output_tokens(), NonZeroU32::new(128));
    assert_eq!(prefix.instructions(), source.instructions());
    Ok(())
}

#[test]
fn compaction_summary_segment_concatenates_assistant_text_across_items() -> TestResult {
    let identity = DurableCompactionIdentity::new(
        "compact-fold-1",
        DurableSessionAddress::new(
            SessionId::try_from(uuid::Uuid::from_u128(11))?,
            BranchId::try_from(uuid::Uuid::from_u128(12))?,
            CheckpointId::try_from(uuid::Uuid::from_u128(13))?,
        ),
    )?;
    let digest = Digest::sha256(DigestDomain::RequestPlan, 1, &[b"compact-fold"])?;
    let first = ConversationItem::new(
        EntryId::try_from(uuid::Uuid::from_u128(21))?,
        Actor::Assistant,
        vec![ContentBlock::Text(TextBlock::new("alpha", None)?)],
        Provenance::ModelOutput,
    )?;
    let second = ConversationItem::new(
        EntryId::try_from(uuid::Uuid::from_u128(22))?,
        Actor::Assistant,
        vec![ContentBlock::Text(TextBlock::new("bravo", None)?)],
        Provenance::ModelOutput,
    )?;
    let response = ModelResponse::new(
        vec![first, second],
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        digest,
    );
    let summary = compaction_summary_segment(identity, &response)?;
    assert_eq!(summary.kind(), HistorySegmentKind::Compaction);
    let ContentBlock::Compaction(block) = &summary.items()[0].blocks()[0] else {
        panic!("expected compaction block");
    };
    assert_eq!(block.summary(), "alpha\nbravo");
    Ok(())
}

#[tokio::test]
async fn a_summary_cut_off_at_its_output_bound_is_rejected_and_publishes_nothing() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let (agent, source) = seeded_compaction_source(
        &sessions,
        Arc::clone(&workspace),
        "create:length-summary",
        91_000,
    )
    .await?;
    let identity = DurableCompactionIdentity::new("compact:length-summary", source)?;
    let (builder, model) = recovery_compaction_fixture(
        workspace,
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::LengthSummary,
        None,
    )?;
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            "compact:length-summary".to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|_| "length summary compaction rejected exact source")?;
    // A truncated summary would replace the history with less than it
    // claims to hold, so the fold is a definite failure: one dispatch,
    // nothing published, the source untouched.
    let AgentCompactSessionOutcome::Failed { diagnostic, .. } = &outcome else {
        return Err(format!("a cut-off summary must fail, got {outcome:?}").into());
    };
    assert!(
        diagnostic
            .as_deref()
            .is_some_and(|text| text.contains("cut off at its output bound")),
        "{diagnostic:?}"
    );
    assert_eq!(model.starts.load(Ordering::Acquire), 1);
    assert!(
        !lock_mutex(&store.state)
            .commits
            .contains_key(&identity.success_checkpoint)
    );
    agent.shutdown().await?;
    Ok(())
}

fn native_compact_test_descriptor(
    route: &str,
    observed_at: Timestamp,
) -> TestResult<ResolvedModelDescriptor> {
    let provenance = FactProvenance::new(FactSource::TestFixture, observed_at, None);
    let features = BTreeMap::from([
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
        (
            Capability::Caching,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                provenance,
            ),
        ),
        (
            Capability::Reasoning,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                provenance,
            ),
        ),
    ]);
    let descriptor = ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("test.protocol")?,
        RouteId::new(route)?,
        ModelId::new("test.model")?,
        None,
        Some(ModelFamilyId::new("test.family")?),
        ModelAdapterProfileId::new("test.profile")?,
        CapabilitySet::new(features, BTreeMap::new()),
    );
    let protocol = descriptor.protocol().clone();
    Ok(descriptor.with_context_contract(durable_test_context_contract(protocol, observed_at)?))
}

fn configured_native_compact_model(
    route: &str,
    observed_at: Timestamp,
    generation: u64,
    registry: &FrozenToolRegistry,
) -> TestResult<ConfiguredLanguageModel> {
    let descriptor = native_compact_test_descriptor(route, observed_at)?;
    let authority = artifact_authority(&descriptor, generation)?;
    let model: Arc<dyn LanguageModel> = Arc::new(ArtifactModel::new(
        descriptor.clone(),
        authority.clone(),
        1,
        1,
        false,
    ));
    configured_test_model_for_operation(
        model,
        descriptor,
        authority,
        registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
        1,
        ReasoningEffortDisposition::Enabled(ReasoningEffort::Medium),
    )
}

#[test]
fn refreshed_harness_compact_does_not_enroll_native_facet() -> TestResult {
    let registry = one_mutating_tool_registry()?;
    let source =
        configured_native_compact_model("test.route", Timestamp::new(1, 0)?, 1, &registry)?;
    let live =
        configured_native_compact_model("test.route", Timestamp::new(100, 0)?, 2, &registry)?;
    assert_ne!(
        live.configuration().effective().descriptor(),
        source.configuration().effective().descriptor()
    );
    assert!(!native_compaction_facet_enrolled(live.configuration()));
    compaction_runtime_matches_source_operation(
        live.configuration(),
        source.configuration(),
        RuntimeDurability::Required,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    Ok(())
}

#[test]
fn native_compact_rejects_a_different_route() -> TestResult {
    let registry = one_mutating_tool_registry()?;
    let source =
        configured_native_compact_model("test.route", Timestamp::new(1, 0)?, 1, &registry)?;
    let live = configured_native_compact_model("other.route", Timestamp::new(1, 0)?, 1, &registry)?;
    let error = compaction_runtime_matches_source_operation(
        live.configuration(),
        source.configuration(),
        RuntimeDurability::Required,
        ContextAdmissionRequirement::AnyBounded,
    )
    .expect_err("foreign route must not compact this source");
    assert_eq!(error.kind(), AgentErrorKind::InvalidInput);
    Ok(())
}

pub(super) fn native_compact_test_descriptor_with_facet(
    route: &str,
    observed_at: Timestamp,
) -> TestResult<ResolvedModelDescriptor> {
    let provenance = FactProvenance::new(FactSource::TestFixture, observed_at, None);
    let features = BTreeMap::from([
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
        (
            Capability::Caching,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                provenance,
            ),
        ),
        (
            Capability::Reasoning,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                provenance,
            ),
        ),
    ]);
    let compaction_namespace = NamespaceId::new("plakous.compaction")?;
    let native_namespaces = BTreeMap::from([(
        compaction_namespace,
        FeatureSupport::new(
            Support::Native,
            CapabilityConstraints::default(),
            provenance,
        ),
    )]);
    let descriptor = ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("test.protocol")?,
        RouteId::new(route)?,
        ModelId::new("test.model")?,
        None,
        Some(ModelFamilyId::new("test.family")?),
        ModelAdapterProfileId::new("test.profile")?,
        CapabilitySet::new(features, native_namespaces),
    );
    let protocol = descriptor.protocol().clone();
    Ok(descriptor.with_context_contract(durable_test_context_contract(protocol, observed_at)?))
}

fn native_compaction_fixture(
    workspace: Arc<WorkspacePolicy>,
) -> TestResult<(AgentBuilder, Arc<ArtifactModel>)> {
    let observed_at = Timestamp::new(10, 0)?;
    let descriptor = native_compact_test_descriptor_with_facet("test.route", observed_at)?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(ArtifactModel::new(
        descriptor.clone(),
        authority.clone(),
        1,
        95_500,
        false,
    ));
    let runtime_model: Arc<dyn LanguageModel> = model.clone();
    let registry = ToolRegistryBuilder::default().freeze();
    let configured = configured_test_model(
        runtime_model,
        descriptor,
        authority,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let builder = AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(1_024), Vec::new())?,
            ReasoningPolicy::default(),
        ))
        .identity_sources(
            Arc::new(FixedClock::new(observed_at)),
            Arc::new(SystemIdGenerator),
        );
    Ok((builder, model))
}

fn compaction_prepare_context(
    observations: &[PrepareContextObservation],
    attempt_id: AttemptId,
) -> TestResult<(Option<ModelCacheAffinity>, Option<ActiveFamilyArtifact>)> {
    observations
        .iter()
        .find(|(id, _, _)| *id == attempt_id)
        .map(|(_, affinity, artifact)| (affinity.clone(), artifact.clone()))
        .ok_or_else(|| {
            let observed = observations
                .iter()
                .map(|(id, _, _)| format!("{id:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("prepare for attempt {attempt_id:?} was not observed; saw [{observed}]").into()
        })
}

#[tokio::test]
async fn native_compaction_dispatch_carries_source_affinity_without_artifact() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let semantic_store: Arc<dyn SessionStore> = store;
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let (agent, source) = seeded_compaction_source(
        &sessions,
        Arc::clone(&workspace),
        "create:native-dispatch-affinity",
        95_500,
    )
    .await?;
    let operation_id = "compact:native-dispatch-affinity";
    let identity = DurableCompactionIdentity::new(operation_id, source)?;
    let source_checkpoint = agent.checkpoint();
    let expected_affinity = source_checkpoint
        .manifest()
        .canonical()
        .context()
        .affinity()
        .clone();
    let expected_artifact = source_checkpoint.manifest().family_artifact().cloned();
    let (builder, model) = native_compaction_fixture(workspace)?;
    let _ = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            operation_id.to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|error| format!("native compaction endpoint failed: {error:?}"))?;
    let observations = model.observed_prepare_contexts();
    let measurement_attempt = compaction_attempt_id(identity.workflow_id, b"source-measurement")?;
    let (dispatch_affinity, dispatch_artifact) =
        compaction_prepare_context(&observations, identity.attempt_id)?;
    assert_ne!(
        identity.attempt_id, measurement_attempt,
        "dispatch and source-measurement attempt ids must differ"
    );
    assert_eq!(
        dispatch_affinity.as_ref(),
        Some(&expected_affinity),
        "native dispatch must carry the source branch cache affinity"
    );
    assert!(
        dispatch_artifact.is_none(),
        "native dispatch must not carry an active family artifact: the trigger prefix \
             differs from the coding stable prefix the artifact is bound to"
    );
    if let Ok((measurement_affinity, measurement_artifact)) =
        compaction_prepare_context(&observations, measurement_attempt)
    {
        assert_eq!(
            measurement_affinity.as_ref(),
            Some(&expected_affinity),
            "when observed, source measurement must carry the live branch cache affinity"
        );
        assert!(
            measurement_artifact.is_none() || expected_artifact.is_some(),
            "measurement artifact observation must match whether the source checkpoint had one"
        );
    }
    Ok(())
}

#[test]
fn semantic_compact_accepts_refreshed_evidence_but_rejects_operation_drift() -> TestResult {
    let registry = ToolRegistryBuilder::default().freeze();
    let source_descriptor = durable_test_descriptor_at("test.route", Timestamp::new(1, 0)?)?;
    let live_descriptor = durable_test_descriptor_at("test.route", Timestamp::new(100, 0)?)?;
    let source_authority = artifact_authority(&source_descriptor, 1)?;
    let live_authority = artifact_authority(&live_descriptor, 2)?;
    let source_model: Arc<dyn LanguageModel> = Arc::new(ArtifactModel::new(
        source_descriptor.clone(),
        source_authority.clone(),
        1,
        1,
        false,
    ));
    let live_model: Arc<dyn LanguageModel> = Arc::new(ArtifactModel::new(
        live_descriptor.clone(),
        live_authority.clone(),
        1,
        1,
        false,
    ));
    let source = configured_test_model_for_operation(
        source_model,
        source_descriptor,
        source_authority,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
        1,
        ReasoningEffortDisposition::Disabled,
    )?;
    let live = configured_test_model_for_operation(
        live_model,
        live_descriptor,
        live_authority,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
        1,
        ReasoningEffortDisposition::Disabled,
    )?;
    assert_ne!(
        source.configuration().effective().descriptor(),
        live.configuration().effective().descriptor()
    );
    assert_eq!(
        source.configuration().selected_identity(),
        live.configuration().selected_identity()
    );
    assert_eq!(
        source.configuration().effective_identity(),
        live.configuration().effective_identity()
    );
    compaction_runtime_matches_source_operation(
        live.configuration(),
        source.configuration(),
        RuntimeDurability::Required,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    for (durability, requirement) in [
        (
            RuntimeDurability::Disabled,
            ContextAdmissionRequirement::AnyBounded,
        ),
        (
            RuntimeDurability::Required,
            ContextAdmissionRequirement::ReleaseVerifiedRequired,
        ),
    ] {
        assert!(
            compaction_runtime_matches_source_operation(
                live.configuration(),
                source.configuration(),
                durability,
                requirement
            )
            .is_err()
        );
    }
    for drift in ["route", "credential", "behavior", "family-policy"] {
        let descriptor = durable_test_descriptor_at(
            if drift == "route" {
                "foreign.route"
            } else {
                "test.route"
            },
            Timestamp::new(100, 0)?,
        )?;
        let authority = if drift == "credential" {
            RuntimeModelAuthority::new(
                &descriptor,
                source
                    .configuration()
                    .effective()
                    .authority()
                    .account_binding(),
                Digest::sha256(DigestDomain::ModelBinding, 1, &[b"changed-credential"])?,
                2,
                Timestamp::new(9, 0)?,
                Some(Timestamp::new(20, 0)?),
            )?
        } else {
            artifact_authority(&descriptor, 2)?
        };
        let model: Arc<dyn LanguageModel> = Arc::new(ArtifactModel::new(
            descriptor.clone(),
            authority.clone(),
            1,
            1,
            false,
        ));
        let changed = configured_test_model_for_operation_with_tools(
            model,
            descriptor,
            authority,
            &registry,
            LocalDurabilityDisposition::DurableRequired,
            ContextAdmissionRequirement::AnyBounded,
            if drift == "family-policy" { 2 } else { 1 },
            ReasoningEffortDisposition::Disabled,
            None,
            ToolPresentationMode::Direct,
            drift != "behavior",
        )?;
        assert!(
            compaction_runtime_matches_source_operation(
                changed.configuration(),
                source.configuration(),
                RuntimeDurability::Required,
                ContextAdmissionRequirement::AnyBounded
            )
            .is_err(),
            "accepted {drift} drift"
        );
    }
    Ok(())
}
