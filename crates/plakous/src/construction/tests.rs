use std::collections::BTreeMap;

use plakous_model::{
    CachePolicy, Capability, CapabilityConstraints, CapabilitySet, CapacityCertificate,
    CapacityRule, CompiledProviderConfiguration, ConfiguredTool, ContextAdmissionRequirement,
    ContextContract, ContextEvidenceSource, CountCertificateKind, CredentialSelectionId, Digest,
    DigestDomain, EffectiveProviderConfiguration, EvidenceFreshness, FactProvenance, FactSource,
    FamilyPolicyEnvelope, FeatureSupport, LocalDurabilityDisposition, MeasurementCertificate,
    ModelAdapterProfileId, ModelFamilyId, ModelId, ModelImplementationIdentity, NamespaceId,
    OutputBoundDisposition, OutputPolicy, ParallelToolDisposition, ProcessingTierDisposition,
    ProtocolId, ProviderAuthorityClassId, ProviderBehaviorConfiguration, ProviderProfileRevisionId,
    ProviderRouteSelection, ProviderSemanticEvidence, ProviderStorageDisposition,
    ReasoningEffortDisposition, ReasoningIncludeDisposition, ReasoningSummaryDisposition,
    ReplayDisposition, ResolvedModelDescriptor, ResolvedProviderAuthorityBinding,
    ResponseStateDisposition, RetentionDisposition, RouteId, RuntimeModelAuthority,
    SelectedProviderConfiguration, Support, Timestamp, ToolChoice, ToolPresentationMode,
};

use super::{CustomProviderConfigurationRequest, provider_model_projection_identity};

type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

const PROTOCOL: &str = "consumer-protocol";
const ROUTE: &str = "consumer-route";
const MODEL: &str = "consumer-model";
const FAMILY: &str = "consumer-family";
const PROFILE: &str = "consumer-profile";
const SEMANTICS: &[u8] = b"consumer-semantics-v1";
const FAMILY_POLICY: &[u8] = b"consumer-family-policy-v1";

/// Builds the same Custom descriptor and authority the fixture builds.
fn descriptor_and_authority() -> Fallible<(ResolvedModelDescriptor, RuntimeModelAuthority)> {
    let time = Timestamp::new(1, 0)?;
    let features = [Capability::Tools, Capability::Streaming]
        .into_iter()
        .map(|capability| {
            (
                capability,
                FeatureSupport::new(
                    Support::Native,
                    CapabilityConstraints::default(),
                    FactProvenance::new(FactSource::TestFixture, time, None),
                ),
            )
        })
        .collect();
    let descriptor = ResolvedModelDescriptor::new_custom(
        ProtocolId::new(PROTOCOL)?,
        RouteId::new(ROUTE)?,
        ModelId::new(MODEL)?,
        None,
        Some(ModelFamilyId::new(FAMILY)?),
        ModelAdapterProfileId::new(PROFILE)?,
        CapabilitySet::new(features, BTreeMap::new()),
    );
    let binding = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"construction-helper"])?;
    let authority = RuntimeModelAuthority::new(&descriptor, binding, binding, 1, time, None)?;
    let class = authority.authority_class_binding()?;
    let freshness = EvidenceFreshness::new(time, None)?;
    let descriptor = descriptor.with_context_contract(ContextContract::new(
        ProtocolId::new(PROTOCOL)?,
        "consumer-accounting-v1",
        "consumer-projection-v1",
        "consumer-reservation-v1",
        CapacityCertificate::custom_envelope(
            "consumer-capacity",
            "v1",
            CapacityRule::ProviderReservedInput {
                reported_context_window: None,
                maximum_projected_input: 1_000_000,
                auto_compact_input: None,
            },
            class,
            freshness,
            binding,
            "v1",
        )?,
        vec![MeasurementCertificate::new(
            "consumer-counter",
            "v1",
            "wire-bytes",
            "v1",
            ContextEvidenceSource::CallerDeclaration,
            CountCertificateKind::CustomEnvelopeUpperBound {
                declaration: binding,
            },
            Some(class),
            freshness,
        )?],
    )?);
    let authority = RuntimeModelAuthority::new(&descriptor, binding, binding, 1, time, None)?;
    Ok((descriptor, authority))
}

/// Returns the behavior both construction paths share.
fn behavior(descriptor: &ResolvedModelDescriptor) -> Fallible<ProviderBehaviorConfiguration> {
    Ok(ProviderBehaviorConfiguration {
        processing_tier: ProcessingTierDisposition::Standard,
        tool_mode: ToolPresentationMode::Direct,
        reasoning_effort: ReasoningEffortDisposition::Disabled,
        reasoning_summary: ReasoningSummaryDisposition::Omit,
        reasoning_include: ReasoningIncludeDisposition::Omit,
        replay: ReplayDisposition::Disabled,
        cache: CachePolicy::Disabled,
        tools: Vec::<ConfiguredTool>::new(),
        hosted_tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        parallel_tools: ParallelToolDisposition::Disabled,
        output: OutputPolicy::default(),
        output_bound: OutputBoundDisposition::NoRequestBound,
        storage: ProviderStorageDisposition::OmittedRouteContract,
        retention: RetentionDisposition::RouteContract {
            profile: descriptor.profile().clone(),
            revision: ProviderProfileRevisionId::new("consumer-retention-v1")?,
        },
        response_state: ResponseStateDisposition::Stateless,
        context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
        local_durability: LocalDurabilityDisposition::DurableRequired,
    })
}

/// Returns the caller's model implementation identity.
fn implementation() -> Fallible<ModelImplementationIdentity> {
    Ok(ModelImplementationIdentity::custom(
        NamespaceId::new("consumer.model")?,
        1,
        NamespaceId::new("consumer.fixture")?,
    )?)
}

/// Compiles the configuration the long way, exactly as a caller did before.
fn hand_built(
    descriptor: &ResolvedModelDescriptor,
    authority: &RuntimeModelAuthority,
) -> Fallible<CompiledProviderConfiguration> {
    let behavior = behavior(descriptor)?;
    let selected = SelectedProviderConfiguration::new(
        ProviderRouteSelection::new(
            descriptor.protocol().clone(),
            descriptor.route().clone(),
            descriptor.profile().clone(),
            ProviderProfileRevisionId::new("consumer-route-v1")?,
            ModelAdapterProfileId::new("consumer.auth")?,
            ProviderProfileRevisionId::new("consumer-auth-v1")?,
            ProviderAuthorityClassId::new("consumer-scripted")?,
            CredentialSelectionId::new("consumer-credential")?,
            descriptor.requested_model().clone(),
        ),
        behavior.clone(),
    )?;
    let stable = ResolvedProviderAuthorityBinding::new(
        descriptor,
        authority,
        ProviderSemanticEvidence::new(1, SEMANTICS.to_vec())?,
    )?;
    let family = FamilyPolicyEnvelope::new(
        descriptor.model_family().cloned().ok_or("missing family")?,
        1,
        1,
        1,
        FAMILY_POLICY.to_vec(),
    )?;
    let effective = EffectiveProviderConfiguration::compile(
        &selected,
        descriptor.clone(),
        implementation()?,
        stable,
        behavior,
        family,
    )?;
    Ok(CompiledProviderConfiguration::new(selected, effective)?)
}

/// Compiles the same configuration through the construction helper.
fn helper_built(
    descriptor: &ResolvedModelDescriptor,
    authority: &RuntimeModelAuthority,
) -> Fallible<CompiledProviderConfiguration> {
    Ok(CustomProviderConfigurationRequest {
        descriptor,
        authority,
        behavior: behavior(descriptor)?,
        integration_revision: ProviderProfileRevisionId::new("consumer-route-v1")?,
        auth_profile: ModelAdapterProfileId::new("consumer.auth")?,
        auth_revision: ProviderProfileRevisionId::new("consumer-auth-v1")?,
        authority_class: ProviderAuthorityClassId::new("consumer-scripted")?,
        credential_selection: CredentialSelectionId::new("consumer-credential")?,
        implementation: implementation()?,
        semantic_evidence: ProviderSemanticEvidence::new(1, SEMANTICS.to_vec())?,
        family_policy_schema_revision: 1,
        family_policy_compiler_revision: 1,
        family_policy_dialect_revision: 1,
        family_policy_bytes: FAMILY_POLICY.to_vec(),
    }
    .compile()?)
}

#[test]
fn the_helper_compiles_the_configuration_the_long_form_compiles() -> Fallible<()> {
    let (descriptor, authority) = descriptor_and_authority()?;
    let hand = hand_built(&descriptor, &authority)?;
    let helper = helper_built(&descriptor, &authority)?;
    assert_eq!(hand.selected_identity(), helper.selected_identity());
    assert_eq!(hand.effective_identity(), helper.effective_identity());
    assert_eq!(
        hand.effective().effective_model(),
        helper.effective().effective_model()
    );
    assert_eq!(
        hand.effective().descriptor().binding_digest()?,
        helper.effective().descriptor().binding_digest()?
    );
    Ok(())
}

#[test]
fn the_helper_derives_the_projection_identity_a_caller_restated() -> Fallible<()> {
    let (descriptor, authority) = descriptor_and_authority()?;
    let configuration = hand_built(&descriptor, &authority)?;
    let effective = configuration.effective();
    let effective_descriptor = effective.descriptor();
    let restated = plakous_tools::ProviderModelProjectionIdentityV2::new(
        effective_descriptor.integration_class(),
        effective_descriptor.protocol().as_str(),
        effective_descriptor.route().as_str(),
        effective_descriptor.profile().as_str(),
        effective_descriptor
            .model_family()
            .ok_or("missing family")?
            .as_str(),
        effective_descriptor.requested_model().as_str(),
        effective.effective_model().as_str(),
        configuration.effective_identity(),
        plakous_tools::PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    assert_eq!(
        restated,
        provider_model_projection_identity(&configuration)?
    );
    Ok(())
}
