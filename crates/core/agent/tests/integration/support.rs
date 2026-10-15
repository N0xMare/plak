//! Model, provider and identity fixtures shared by the scripted agent suites.
//!
//! Each suite names its own [`Fixture`], so the identities its digests are
//! built from stay distinct from every other suite's.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::Arc;

use plakous_model::{
    Actor, CachePolicy, Capability, CapabilityConstraints, CapabilitySet, CapacityCertificate,
    CapacityRule, CompiledProviderConfiguration, ConfiguredTool, ContentBlock,
    ContextAdmissionRequirement, ContextContract, ContextEvidenceSource, ConversationItem,
    CountCertificateKind, CountObservation, CredentialSelectionId, EffectiveProviderConfiguration,
    EntryId, EvidenceFreshness, FactProvenance, FactSource, FamilyPolicyEnvelope, FeatureSupport,
    FixedClock, GenerationLimits, LocalDurabilityDisposition, MeasurementCertificate,
    ModelAdapterProfileId, ModelFamilyId, ModelId, ModelImplementationIdentity,
    OutputBoundDisposition, OutputPolicy, ParallelToolDisposition, ProtocolId, Provenance,
    ProviderAuthorityClassId, ProviderBehaviorConfiguration, ProviderProfileRevisionId,
    ProviderRouteSelection, ProviderSemanticEvidence, ProviderStorageDisposition,
    ReasoningEffortDisposition, ReasoningIncludeDisposition, ReasoningSummaryDisposition,
    ReplayDisposition, ResolvedModelDescriptor, ResolvedProviderAuthorityBinding,
    ResponseStateDisposition, RetentionDisposition, RouteId, RuntimeModelAuthority, SafeLimit,
    SelectedProviderConfiguration, SequenceIdGenerator, Support, TextBlock, TextPhase, Timestamp,
    ToolChoice, ToolDefinition, ToolExecutionDisposition,
};
use plakous_tools::WorkspacePolicy;
use tempfile::TempDir;
use uuid::Uuid;

pub(crate) type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

pub(crate) const MAX_OUTPUT_TOKENS: u32 = 128;

/// The identities one suite builds its provider configuration and context
/// evidence from.
pub(crate) struct Fixture {
    /// Prefix of every scripted evidence, authority and provider-profile id.
    pub(crate) label: &'static str,
    pub(crate) provider_semantics: &'static [u8],
}

/// The model identities of one suite's scripted descriptor.
pub(crate) struct DescriptorIds {
    pub(crate) protocol: &'static str,
    pub(crate) route: &'static str,
    pub(crate) model: &'static str,
    pub(crate) family: &'static str,
    pub(crate) profile: &'static str,
}

impl Fixture {
    fn scripted(&self, suffix: &str) -> String {
        format!("{}-scripted-{suffix}", self.label)
    }

    fn labelled(&self, separator: &str, suffix: &str) -> String {
        format!("{}{separator}{suffix}", self.label)
    }
}

pub(crate) fn id<T>(value: u128) -> TestResult<T>
where
    T: TryFrom<Uuid>,
    T::Error: std::error::Error + 'static,
{
    Ok(T::try_from(Uuid::from_u128(value))?)
}

pub(crate) fn timestamp() -> TestResult<Timestamp> {
    Ok(Timestamp::new(1, 0)?)
}

pub(crate) fn context_contract(
    fixture: &Fixture,
    protocol: ProtocolId,
) -> TestResult<ContextContract> {
    let freshness = EvidenceFreshness::new(timestamp()?, None)?;
    Ok(ContextContract::new(
        protocol,
        "scripted-accounting-v1",
        "scripted-projection-v1",
        "scripted-reservation-v1",
        CapacityCertificate::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            fixture.scripted("capacity"),
            fixture.scripted("capacity-v1"),
            ContextEvidenceSource::BuiltInCatalog,
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(1_000_000_000),
            },
            None,
            freshness,
        )?,
        vec![MeasurementCertificate::standard(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            fixture.scripted("counter"),
            fixture.scripted("exact-v1"),
            fixture.scripted("complete-projection"),
            "scripted-lowering-v1",
            ContextEvidenceSource::BuiltInCatalog,
            CountCertificateKind::ExactTokenizer,
            None,
            freshness,
        )?],
    )?)
}

pub(crate) fn context_observation(fixture: &Fixture) -> TestResult<CountObservation> {
    Ok(CountObservation::new(
        fixture.scripted("counter"),
        fixture.scripted("exact-v1"),
        64,
    )?)
}

pub(crate) fn generation_limits() -> TestResult<GenerationLimits> {
    Ok(GenerationLimits::new(
        NonZeroU32::new(MAX_OUTPUT_TOKENS),
        Vec::new(),
    )?)
}

pub(crate) fn runtime_authority(
    fixture: &Fixture,
    descriptor: &ResolvedModelDescriptor,
) -> TestResult<RuntimeModelAuthority> {
    let account = plakous_model::Digest::sha256(
        plakous_model::DigestDomain::ModelBinding,
        1,
        &[fixture.scripted("account").as_bytes()],
    )?;
    let credential = plakous_model::Digest::sha256(
        plakous_model::DigestDomain::ModelBinding,
        1,
        &[fixture.scripted("credential").as_bytes()],
    )?;
    Ok(RuntimeModelAuthority::new(
        descriptor,
        account,
        credential,
        1,
        timestamp()?,
        None,
    )?)
}

/// A scripted test-fixture descriptor with native support for exactly
/// `capabilities` and the fixture's context contract.
pub(crate) fn descriptor(
    fixture: &Fixture,
    ids: &DescriptorIds,
    capabilities: &[Capability],
) -> TestResult<ResolvedModelDescriptor> {
    let provenance = FactProvenance::new(FactSource::TestFixture, timestamp()?, None);
    let features = capabilities
        .iter()
        .map(|capability| {
            (
                *capability,
                FeatureSupport::new(
                    Support::Native,
                    CapabilityConstraints::default(),
                    provenance,
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let protocol = ProtocolId::new(ids.protocol)?;
    Ok(ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        protocol.clone(),
        RouteId::new(ids.route)?,
        ModelId::new(ids.model)?,
        None,
        Some(ModelFamilyId::new(ids.family)?),
        ModelAdapterProfileId::new(ids.profile)?,
        CapabilitySet::new(features, BTreeMap::new()),
    )
    .with_context_contract(context_contract(fixture, protocol)?))
}

/// A direct-tool, stateless, ephemeral provider configuration; suites vary
/// only the configured tools, cache policy and output bound.
pub(crate) fn provider_configuration(
    fixture: &Fixture,
    descriptor: &ResolvedModelDescriptor,
    authority: &RuntimeModelAuthority,
    tools: &[ToolDefinition],
    cache: CachePolicy,
    output_bound: OutputBoundDisposition,
) -> TestResult<CompiledProviderConfiguration> {
    let behavior = ProviderBehaviorConfiguration {
        processing_tier: plakous_model::ProcessingTierDisposition::Standard,
        tool_mode: plakous_model::ToolPresentationMode::Direct,
        reasoning_effort: ReasoningEffortDisposition::Disabled,
        reasoning_summary: ReasoningSummaryDisposition::Omit,
        reasoning_include: ReasoningIncludeDisposition::Omit,
        replay: ReplayDisposition::Disabled,
        cache,
        tools: tools
            .iter()
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
        output_bound,
        storage: ProviderStorageDisposition::OmittedRouteContract,
        retention: RetentionDisposition::RouteContract {
            profile: descriptor.profile().clone(),
            revision: ProviderProfileRevisionId::new(fixture.labelled("-", "retention-v1"))?,
        },
        response_state: ResponseStateDisposition::Stateless,
        context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
        local_durability: LocalDurabilityDisposition::Ephemeral,
    };
    let selected = SelectedProviderConfiguration::new(
        ProviderRouteSelection::new(
            descriptor.protocol().clone(),
            descriptor.route().clone(),
            descriptor.profile().clone(),
            ProviderProfileRevisionId::new(fixture.labelled("-", "route-v1"))?,
            ModelAdapterProfileId::new(fixture.labelled(".", "auth"))?,
            ProviderProfileRevisionId::new(fixture.labelled("-", "auth-v1"))?,
            ProviderAuthorityClassId::new(fixture.labelled("-", "scripted"))?,
            CredentialSelectionId::new(fixture.labelled("-", "credential"))?,
            descriptor.requested_model().clone(),
        ),
        behavior.clone(),
    )?;
    let stable_authority = ResolvedProviderAuthorityBinding::new(
        descriptor,
        authority,
        ProviderSemanticEvidence::new(1, fixture.provider_semantics.to_vec())?,
    )?;
    let family_policy = FamilyPolicyEnvelope::new(
        descriptor
            .model_family()
            .cloned()
            .ok_or("fixture descriptor requires a model family")?,
        1,
        1,
        1,
        fixture.labelled("-", "family-policy-v1").into_bytes(),
    )?;
    let effective = EffectiveProviderConfiguration::compile(
        &selected,
        descriptor.clone(),
        ModelImplementationIdentity::release_owned(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            descriptor,
            1,
        )?,
        stable_authority,
        behavior,
        family_policy,
    )?;
    Ok(CompiledProviderConfiguration::new(selected, effective)?)
}

pub(crate) fn scripted_model(
    fixture: &Fixture,
    descriptor: ResolvedModelDescriptor,
    scenarios: impl IntoIterator<Item = plakous_test_support::ModelScenario>,
) -> TestResult<plakous_test_support::ScriptedModel> {
    let authority = runtime_authority(fixture, &descriptor)?;
    Ok(
        plakous_test_support::ScriptedModel::new(descriptor, scenarios)
            .with_runtime_authority(authority)
            .with_context_observation(context_observation(fixture)?),
    )
}

pub(crate) fn workspace() -> TestResult<(TempDir, Arc<WorkspacePolicy>)> {
    let directory = tempfile::tempdir()?;
    let policy = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    Ok((directory, policy))
}

/// A fixed clock and a generator over the first `ids` sequential UUIDs.
pub(crate) fn identity_sources(
    ids: u128,
) -> TestResult<(Arc<FixedClock>, Arc<SequenceIdGenerator>)> {
    Ok((
        Arc::new(FixedClock::new(timestamp()?)),
        Arc::new(SequenceIdGenerator::new(
            (1_u128..=ids).map(Uuid::from_u128),
        )?),
    ))
}

pub(crate) fn user_text_item(entry: u128, text: &str) -> TestResult<ConversationItem> {
    Ok(ConversationItem::new(
        id::<EntryId>(entry)?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new(text, None)?)],
        Provenance::UserInput,
    )?)
}

pub(crate) fn assistant_text_item(entry: u128, text: &str) -> TestResult<ConversationItem> {
    Ok(ConversationItem::new(
        id::<EntryId>(entry)?,
        Actor::Assistant,
        vec![ContentBlock::Text(TextBlock::new(
            text,
            Some(TextPhase::Final),
        )?)],
        Provenance::ModelOutput,
    )?)
}
