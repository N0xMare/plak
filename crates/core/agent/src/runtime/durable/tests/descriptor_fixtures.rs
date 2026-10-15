use super::*;

pub(super) fn durable_test_context_contract(
    protocol: ProtocolId,
    observed_at: Timestamp,
) -> TestResult<ContextContract> {
    let freshness = EvidenceFreshness::new(observed_at, None)?;
    Ok(ContextContract::new(
        protocol,
        "artifact-accounting-v1",
        "artifact-projection-v1",
        "artifact-reservation-v1",
        CapacityCertificate::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            "agent-scripted-capacity",
            "agent-scripted-capacity-v1",
            ContextEvidenceSource::BuiltInCatalog,
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(1_000_000),
            },
            None,
            freshness,
        )?,
        vec![MeasurementCertificate::standard(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            "agent-scripted-counter",
            "agent-scripted-exact-v1",
            "agent-scripted-complete-projection",
            "agent-scripted-method-v1",
            ContextEvidenceSource::BuiltInCatalog,
            CountCertificateKind::ExactTokenizer,
            None,
            freshness,
        )?],
    )?)
}

pub(super) fn durable_test_descriptor(route: &str) -> TestResult<ResolvedModelDescriptor> {
    durable_test_descriptor_at(route, Timestamp::new(1, 0)?)
}

pub(super) fn durable_test_descriptor_at(
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
            Capability::Caching,
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

pub(super) fn custom_artifact_test_descriptor(route: &str) -> TestResult<ResolvedModelDescriptor> {
    let base = durable_test_descriptor(route)?;
    let freshness = EvidenceFreshness::new(Timestamp::new(1, 0)?, None)?;
    let authority_binding = RuntimeModelAuthority::authority_class_for(
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"artifact-account"])?,
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"artifact-credential-authority"],
        )?,
    )?;
    let declaration = Digest::sha256(
        DigestDomain::ModelBinding,
        1,
        &[b"custom artifact fixture complete-projection upper bound"],
    )?;
    let contract = ContextContract::new(
        base.protocol().clone(),
        "artifact-accounting-v1",
        "artifact-projection-v1",
        "artifact-reservation-v1",
        CapacityCertificate::custom_envelope(
            "agent-custom-artifact-capacity",
            "agent-custom-artifact-capacity-v1",
            CapacityRule::ProviderReservedInput {
                reported_context_window: None,
                maximum_projected_input: 1_000_000,
                auto_compact_input: None,
            },
            authority_binding,
            freshness,
            declaration,
            "agent-custom-artifact-declaration-v1",
        )?,
        vec![MeasurementCertificate::new(
            "agent-scripted-counter",
            "agent-scripted-exact-v1",
            "agent-scripted-complete-projection",
            "agent-scripted-method-v1",
            ContextEvidenceSource::CallerDeclaration,
            CountCertificateKind::CustomEnvelopeUpperBound { declaration },
            Some(authority_binding),
            freshness,
        )?],
    )?;
    Ok(ResolvedModelDescriptor::new_custom(
        base.protocol().clone(),
        base.route().clone(),
        base.requested_model().clone(),
        base.reported_model().cloned(),
        base.model_family().cloned(),
        base.profile().clone(),
        base.capabilities().clone(),
    )
    .with_context_contract(contract))
}

pub(super) fn configured_test_model(
    model: Arc<dyn LanguageModel>,
    descriptor: ResolvedModelDescriptor,
    authority: RuntimeModelAuthority,
    registry: &FrozenToolRegistry,
    durability: LocalDurabilityDisposition,
    context_evidence_requirement: ContextAdmissionRequirement,
) -> TestResult<ConfiguredLanguageModel> {
    configured_test_model_for_operation(
        model,
        descriptor,
        authority,
        registry,
        durability,
        context_evidence_requirement,
        1,
        ReasoningEffortDisposition::Disabled,
    )
}

fn configured_test_model_with_family_revision(
    model: Arc<dyn LanguageModel>,
    descriptor: ResolvedModelDescriptor,
    authority: RuntimeModelAuthority,
    registry: &FrozenToolRegistry,
    durability: LocalDurabilityDisposition,
    context_evidence_requirement: ContextAdmissionRequirement,
    family_revision: u32,
) -> TestResult<ConfiguredLanguageModel> {
    configured_test_model_for_operation(
        model,
        descriptor,
        authority,
        registry,
        durability,
        context_evidence_requirement,
        family_revision,
        ReasoningEffortDisposition::Disabled,
    )
}

#[allow(
    clippy::too_many_arguments,
    clippy::needless_pass_by_value,
    reason = "the fixture retains one explicit complete provider-operation configuration"
)]
pub(super) fn configured_test_model_for_operation(
    model: Arc<dyn LanguageModel>,
    descriptor: ResolvedModelDescriptor,
    authority: RuntimeModelAuthority,
    registry: &FrozenToolRegistry,
    durability: LocalDurabilityDisposition,
    context_evidence_requirement: ContextAdmissionRequirement,
    family_revision: u32,
    reasoning_effort: ReasoningEffortDisposition,
) -> TestResult<ConfiguredLanguageModel> {
    configured_test_model_for_operation_with_tools(
        model,
        descriptor,
        authority,
        registry,
        durability,
        context_evidence_requirement,
        family_revision,
        reasoning_effort,
        None,
        ToolPresentationMode::Direct,
        true,
    )
}

#[allow(
    clippy::too_many_arguments,
    clippy::needless_pass_by_value,
    reason = "the fixture retains one explicit complete provider-operation configuration"
)]
pub(super) fn configured_test_model_for_operation_with_tools(
    model: Arc<dyn LanguageModel>,
    descriptor: ResolvedModelDescriptor,
    authority: RuntimeModelAuthority,
    registry: &FrozenToolRegistry,
    durability: LocalDurabilityDisposition,
    context_evidence_requirement: ContextAdmissionRequirement,
    family_revision: u32,
    reasoning_effort: ReasoningEffortDisposition,
    configured_tool_names: Option<&[&str]>,
    tool_mode: ToolPresentationMode,
    cache_enabled: bool,
) -> TestResult<ConfiguredLanguageModel> {
    let behavior = ProviderBehaviorConfiguration {
        processing_tier: plakous_model::ProcessingTierDisposition::Standard,
        tool_mode,
        reasoning_effort,
        reasoning_summary: ReasoningSummaryDisposition::Omit,
        reasoning_include: ReasoningIncludeDisposition::Omit,
        replay: ReplayDisposition::Disabled,
        cache: if cache_enabled {
            CachePolicy::Enabled {
                requirement: CapabilityRequirement::Required,
            }
        } else {
            CachePolicy::Disabled
        },
        tools: registry
            .definitions()
            .iter()
            .filter(|definition| {
                configured_tool_names
                    .is_none_or(|names| names.contains(&definition.name().as_str()))
            })
            .cloned()
            .map(|definition| {
                ConfiguredTool::new(
                    definition,
                    ToolExecutionDisposition::Local,
                    plakous_model::ToolCallerDisposition::DirectOnly,
                )
            })
            .collect(),
        hosted_tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        parallel_tools: ParallelToolDisposition::Disabled,
        output: OutputPolicy::default(),
        output_bound: OutputBoundDisposition::Enforced(
            NonZeroU32::new(128).ok_or("nonzero configured output bound")?,
        ),
        storage: ProviderStorageDisposition::OmittedRouteContract,
        retention: RetentionDisposition::RouteContract {
            profile: descriptor.profile().clone(),
            revision: ProviderProfileRevisionId::new("agent-test-retention-v1")?,
        },
        response_state: ResponseStateDisposition::Stateless,
        context_evidence_requirement,
        local_durability: durability,
    };
    let selected = SelectedProviderConfiguration::new(
        ProviderRouteSelection::new(
            descriptor.protocol().clone(),
            descriptor.route().clone(),
            descriptor.profile().clone(),
            ProviderProfileRevisionId::new("agent-test-adapter-v1")?,
            ModelAdapterProfileId::new("agent-test.auth")?,
            ProviderProfileRevisionId::new("agent-test-auth-v1")?,
            ProviderAuthorityClassId::new("agent-test")?,
            CredentialSelectionId::new("agent-test-credential")?,
            descriptor.requested_model().clone(),
        ),
        behavior.clone(),
    )?;
    let stable_authority = ResolvedProviderAuthorityBinding::new(
        &descriptor,
        &authority,
        ProviderSemanticEvidence::new(1, b"agent-test-provider-semantics-v1".to_vec())?,
    )?;
    let family_policy = FamilyPolicyEnvelope::new(
        descriptor
            .model_family()
            .cloned()
            .ok_or("agent test descriptor omitted model family")?,
        1,
        1,
        family_revision,
        b"agent-test-family-policy-v1".to_vec(),
    )?;
    let custom = descriptor.integration_class() == plakous_model::IntegrationClass::Custom;
    let implementation = if custom {
        ModelImplementationIdentity::custom(
            NamespaceId::new("agent-test.custom-artifact-model")?,
            1,
            NamespaceId::new("agent-test.runtime-fixture")?,
        )?
    } else {
        ModelImplementationIdentity::release_owned(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            &descriptor,
            1,
        )?
    };
    let effective = EffectiveProviderConfiguration::compile(
        &selected,
        descriptor.clone(),
        implementation,
        stable_authority,
        behavior,
        family_policy,
    )?;
    let configuration = CompiledProviderConfiguration::new(selected, effective)?;
    if custom {
        Ok(ConfiguredLanguageModel::custom(
            model,
            configuration,
            authority,
        )?)
    } else {
        Ok(ConfiguredLanguageModel::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            model,
            configuration,
            authority,
        )?)
    }
}

pub(super) fn reference_client_envelope_test_descriptor(
    route: &str,
) -> TestResult<ResolvedModelDescriptor> {
    let base = durable_test_descriptor(route)?;
    let authority = artifact_authority(&base, 1)?;
    let authority_binding = authority.authority_class_binding()?;
    let freshness = EvidenceFreshness::new(Timestamp::new(1, 0)?, Some(Timestamp::new(20, 0)?))?;
    let receipt = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"agent-reference-catalog"])?;
    let contract = ContextContract::new(
        base.protocol().clone(),
        "artifact-accounting-v1",
        "artifact-projection-v1",
        "artifact-reservation-v1",
        CapacityCertificate::reference_client_envelope(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            "agent-scripted-capacity",
            "agent-reference-v1",
            CapacityRule::ProviderReservedInput {
                reported_context_window: Some(1_000_000),
                maximum_projected_input: 1_000_000,
                auto_compact_input: None,
            },
            authority_binding,
            freshness,
            receipt,
            "agent-reference-v1",
        )?,
        vec![MeasurementCertificate::standard(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            "agent-scripted-counter",
            "agent-scripted-exact-v1",
            "agent-scripted-complete-projection",
            "agent-scripted-method-v1",
            ContextEvidenceSource::AuthenticatedDiscovery,
            CountCertificateKind::ReferenceClientEnvelopeUpperBound {
                catalog_receipt: receipt,
            },
            Some(authority_binding),
            freshness,
        )?],
    )?;
    Ok(base.with_context_contract(contract))
}

pub(super) fn durable_test_builder(
    workspace: Arc<WorkspacePolicy>,
    registry: FrozenToolRegistry,
    route: &str,
    account: &'static [u8],
    policy_changed: bool,
) -> TestResult<AgentBuilder> {
    durable_test_builder_with_family_revision(
        workspace,
        registry,
        route,
        account,
        policy_changed,
        1,
    )
}

pub(super) fn durable_test_builder_with_family_revision(
    workspace: Arc<WorkspacePolicy>,
    registry: FrozenToolRegistry,
    route: &str,
    account: &'static [u8],
    policy_changed: bool,
    family_revision: u32,
) -> TestResult<AgentBuilder> {
    let descriptor = durable_test_descriptor(route)?;
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account", account])?,
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"credential-authority", account],
        )?,
        1,
        Timestamp::new(9, 0)?,
        Some(Timestamp::new(20, 0)?),
    )?;
    let model: Arc<dyn LanguageModel> = Arc::new(
        plakous_test_support::ScriptedModel::new(descriptor.clone(), [])
            .with_runtime_authority(authority.clone())
            .with_context_observation(CountObservation::new(
                "agent-scripted-counter",
                "agent-scripted-exact-v1",
                64,
            )?),
    );
    let configured = configured_test_model_with_family_revision(
        model,
        descriptor,
        authority,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
        family_revision,
    )?;
    let mut builder = AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::default(),
        ))
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        );
    if policy_changed {
        builder = builder.tool_policy(Arc::new(AlwaysAllowPolicy));
    }
    Ok(builder)
}

#[test]
fn configured_runtime_authority_binds_effective_configuration_identity() -> TestResult {
    let descriptor = durable_test_descriptor("test.configured-authority")?;
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credential"])?,
        1,
        Timestamp::new(9, 0)?,
        Some(Timestamp::new(20, 0)?),
    )?;
    let model: Arc<dyn LanguageModel> = Arc::new(
        plakous_test_support::ScriptedModel::new(descriptor.clone(), [])
            .with_runtime_authority(authority.clone()),
    );
    let registry = ToolRegistryBuilder::default().freeze();
    let required = configured_test_model(
        Arc::clone(&model),
        descriptor.clone(),
        authority.clone(),
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let best_effort = configured_test_model(
        model,
        descriptor.clone(),
        authority.clone(),
        &registry,
        LocalDurabilityDisposition::BestEffort,
        ContextAdmissionRequirement::AnyBounded,
    )?;

    let required_binding = validate_configuration_runtime_authority(
        required.configuration(),
        &descriptor,
        &authority,
        Timestamp::new(10, 0)?,
    )?;
    let best_effort_binding = validate_configuration_runtime_authority(
        best_effort.configuration(),
        &descriptor,
        &authority,
        Timestamp::new(10, 0)?,
    )?;
    assert_ne!(required_binding, best_effort_binding);
    assert_eq!(required_binding.runtime(), &authority);
    assert_eq!(
        required_binding.effective_configuration(),
        required.configuration().effective_identity()
    );
    assert_ne!(required_binding.binding(), best_effort_binding.binding());
    assert_eq!(
        required_binding,
        validate_configuration_runtime_authority(
            required.configuration(),
            &descriptor,
            &authority,
            Timestamp::new(11, 0)?,
        )?
    );
    Ok(())
}

pub(super) fn artifact_test_model(
    candidate_calls: usize,
    entry_base: u128,
) -> TestResult<Arc<ArtifactModel>> {
    artifact_test_model_at(candidate_calls, entry_base, Timestamp::new(1, 0)?)
}

pub(super) fn artifact_test_model_at(
    candidate_calls: usize,
    entry_base: u128,
    observed_at: Timestamp,
) -> TestResult<Arc<ArtifactModel>> {
    let descriptor = durable_test_descriptor_at("test.route", observed_at)?;
    let authority = artifact_authority(&descriptor, 1)?;
    Ok(Arc::new(ArtifactModel::new(
        descriptor,
        authority,
        candidate_calls,
        entry_base,
        false,
    )))
}

pub(super) fn artifact_authority(
    descriptor: &ResolvedModelDescriptor,
    generation: u64,
) -> TestResult<RuntimeModelAuthority> {
    Ok(RuntimeModelAuthority::new(
        descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"artifact-account"])?,
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"artifact-credential-authority"],
        )?,
        generation,
        Timestamp::new(9, 0)?,
        Some(Timestamp::new(20, 0)?),
    )?)
}

pub(super) fn tampered_artifact_test_model(entry_base: u128) -> TestResult<Arc<ArtifactModel>> {
    let descriptor = durable_test_descriptor("test.route")?;
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"artifact-account"])?,
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"artifact-credential-authority"],
        )?,
        1,
        Timestamp::new(9, 0)?,
        Some(Timestamp::new(20, 0)?),
    )?;
    Ok(Arc::new(ArtifactModel::new(
        descriptor, authority, 1, entry_base, true,
    )))
}

pub(super) fn foreign_affinity_artifact_test_model(
    entry_base: u128,
) -> TestResult<Arc<ArtifactModel>> {
    let descriptor = durable_test_descriptor("test.route")?;
    let authority = artifact_authority(&descriptor, 1)?;
    Ok(Arc::new(
        ArtifactModel::new(descriptor, authority, 1, entry_base, false).with_tampered_affinity(),
    ))
}

pub(super) fn artifact_test_builder(
    workspace: Arc<WorkspacePolicy>,
    model: Arc<ArtifactModel>,
) -> AgentBuilder {
    artifact_test_builder_with_durability(workspace, model, LocalDurabilityDisposition::Ephemeral)
}

pub(super) fn durable_artifact_test_builder(
    workspace: Arc<WorkspacePolicy>,
    model: Arc<ArtifactModel>,
) -> AgentBuilder {
    artifact_test_builder_with_durability(
        workspace,
        model,
        LocalDurabilityDisposition::DurableRequired,
    )
}

fn artifact_test_builder_with_durability(
    workspace: Arc<WorkspacePolicy>,
    model: Arc<ArtifactModel>,
    durability: LocalDurabilityDisposition,
) -> AgentBuilder {
    let descriptor = model.descriptor.clone();
    let authority = model.authority.clone();
    let model: Arc<dyn LanguageModel> = model;
    let registry = ToolRegistryBuilder::default().freeze();
    let configured = configured_test_model(
        model,
        descriptor,
        authority,
        &registry,
        durability,
        ContextAdmissionRequirement::AnyBounded,
    )
    .expect("valid configured artifact test model");
    AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())
                .expect("valid fixture generation limits"),
            ReasoningPolicy::default(),
        ))
        .identity_sources(
            Arc::new(FixedClock::new(
                Timestamp::new(10, 0).expect("valid fixture timestamp"),
            )),
            Arc::new(SystemIdGenerator),
        )
}

/// Records that it was asked, and answers as constructed.
#[derive(Debug)]
pub(super) struct FakeProjectionAdmission {
    pub(super) admits: bool,
    pub(super) asked: AtomicUsize,
}

impl FakeProjectionAdmission {
    pub(super) fn new(admits: bool) -> Arc<Self> {
        Arc::new(Self {
            admits,
            asked: AtomicUsize::new(0),
        })
    }
}

impl ProjectionCandidateAdmission for FakeProjectionAdmission {
    fn admit(
        &self,
        _job_config: plakous_tools::JobConfig,
        _projection: &ToolProjectionMap,
        _registry: &FrozenToolRegistry,
        _configuration: &CompiledProviderConfiguration,
    ) -> Result<(), ToolRuntimeError> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        if self.admits {
            Ok(())
        } else {
            Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "fake admission refuses this candidate",
            ))
        }
    }
}
