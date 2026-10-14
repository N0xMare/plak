use std::error::Error;

use plakous_model::{
    CachePolicy, CompiledProviderConfiguration, ConfiguredLanguageModel, ConfiguredTool,
    ContextAdmissionRequirement, CredentialSelectionId, EffectiveProviderConfiguration,
    FamilyPolicyEnvelope, LanguageModel, LocalDurabilityDisposition, ModelAdapterProfileId,
    ModelImplementationIdentity, OutputBoundDisposition, OutputPolicy, ParallelToolDisposition,
    ProviderAuthorityClassId, ProviderBehaviorConfiguration, ProviderProfileRevisionId,
    ProviderRouteSelection, ProviderSemanticEvidence, ProviderStorageDisposition,
    ReasoningEffortDisposition, ReasoningIncludeDisposition, ReasoningSummaryDisposition,
    ReplayDisposition, ResolvedProviderAuthorityBinding, ResponseStateDisposition,
    RetentionDisposition, RuntimeModelAuthority, SelectedProviderConfiguration, ToolChoice,
    ToolDefinition, ToolExecutionDisposition,
};
use plakous_model::{
    Capability, CapabilityRequirement, CapacityCertificate, CapacityRule, ContextContract,
    ContextContractError, ContextEvidenceSource, CountCertificateKind, CountObservation,
    EvidenceFreshness, MeasurementCertificate, ProtocolId, ResolvedModelDescriptor, SafeLimit,
    Support, Timestamp,
};
use plakous_model::{Digest, DigestDomain};
use std::sync::Arc;

pub(crate) const TEST_CONTEXT_ACCOUNTING_REVISION: &str = "scripted-accounting-v1";
pub(crate) const TEST_CONTEXT_PROJECTION_REVISION: &str = "scripted-projection-v1";
pub(crate) const TEST_CONTEXT_RESERVATION_REVISION: &str = "scripted-reservation-v1";
pub(crate) const TEST_CONTEXT_COUNTER_ID: &str = "scripted-counter";
pub(crate) const TEST_CONTEXT_COUNTER_REVISION: &str = "scripted-exact-v1";

pub(crate) fn context_contract(
    protocol_family: ProtocolId,
    observed_at: Timestamp,
) -> Result<ContextContract, ContextContractError> {
    context_contract_with_shared_total(protocol_family, observed_at, 1_000_000)
}

pub(crate) fn context_contract_with_shared_total(
    protocol_family: ProtocolId,
    observed_at: Timestamp,
    total: u64,
) -> Result<ContextContract, ContextContractError> {
    let freshness = EvidenceFreshness::new(observed_at, None)?;
    ContextContract::new(
        protocol_family,
        TEST_CONTEXT_ACCOUNTING_REVISION,
        TEST_CONTEXT_PROJECTION_REVISION,
        TEST_CONTEXT_RESERVATION_REVISION,
        CapacityCertificate::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            "scripted-capacity",
            "scripted-capacity-v1",
            ContextEvidenceSource::BuiltInCatalog,
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(total),
            },
            None,
            freshness,
        )?,
        vec![MeasurementCertificate::standard(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            TEST_CONTEXT_COUNTER_ID,
            TEST_CONTEXT_COUNTER_REVISION,
            "scripted-complete-projection",
            "scripted-method-v1",
            ContextEvidenceSource::BuiltInCatalog,
            CountCertificateKind::ExactTokenizer,
            None,
            freshness,
        )?],
    )
}

pub(crate) fn with_context_contract(
    descriptor: ResolvedModelDescriptor,
    observed_at: Timestamp,
) -> Result<ResolvedModelDescriptor, ContextContractError> {
    let protocol_family = descriptor.protocol().clone();
    Ok(descriptor.with_context_contract(context_contract(protocol_family, observed_at)?))
}

#[cfg(feature = "native-subscriptions")]
pub(crate) fn with_shared_window_context_contract(
    descriptor: ResolvedModelDescriptor,
    observed_at: Timestamp,
    total: u64,
) -> Result<ResolvedModelDescriptor, ContextContractError> {
    let protocol_family = descriptor.protocol().clone();
    Ok(
        descriptor.with_context_contract(context_contract_with_shared_total(
            protocol_family,
            observed_at,
            total,
        )?),
    )
}

pub(crate) fn context_observation() -> Result<CountObservation, ContextContractError> {
    CountObservation::new(TEST_CONTEXT_COUNTER_ID, TEST_CONTEXT_COUNTER_REVISION, 64)
}

pub(crate) fn runtime_authority(
    descriptor: &ResolvedModelDescriptor,
    checked_at: Timestamp,
) -> Result<RuntimeModelAuthority, Box<dyn Error>> {
    let account = Digest::sha256(
        DigestDomain::ModelBinding,
        1,
        &[b"plakous-facade-scripted-account"],
    )?;
    let credential = Digest::sha256(
        DigestDomain::ModelBinding,
        1,
        &[b"plakous-facade-scripted-credential"],
    )?;
    Ok(RuntimeModelAuthority::new(
        descriptor, account, credential, 1, checked_at, None,
    )?)
}

pub(crate) fn scripted_model(
    descriptor: ResolvedModelDescriptor,
    scenarios: impl IntoIterator<Item = plakous_test_support::ModelScenario>,
    checked_at: Timestamp,
) -> Result<plakous_test_support::ScriptedModel, Box<dyn Error>> {
    let authority = runtime_authority(&descriptor, checked_at)?;
    Ok(
        plakous_test_support::ScriptedModel::new(descriptor, scenarios)
            .with_runtime_authority(authority)
            .with_context_observation(context_observation()?),
    )
}

pub(crate) fn configured_model(
    model: Arc<dyn LanguageModel>,
    descriptor: ResolvedModelDescriptor,
    authority: RuntimeModelAuthority,
    tools: impl IntoIterator<Item = ToolDefinition>,
    local_durability: LocalDurabilityDisposition,
    output_bound: OutputBoundDisposition,
) -> Result<ConfiguredLanguageModel, Box<dyn Error>> {
    configured_model_for_operation(
        model,
        descriptor,
        authority,
        tools,
        local_durability,
        output_bound,
        plakous_model::ToolPresentationMode::Direct,
    )
}

pub(crate) fn configured_compaction_model(
    model: Arc<dyn LanguageModel>,
    descriptor: ResolvedModelDescriptor,
    authority: RuntimeModelAuthority,
    local_durability: LocalDurabilityDisposition,
) -> Result<ConfiguredLanguageModel, Box<dyn Error>> {
    configured_model(
        model,
        descriptor,
        authority,
        std::iter::empty(),
        local_durability,
        OutputBoundDisposition::Enforced(
            std::num::NonZeroU32::new(128).ok_or("nonzero configured output bound")?,
        ),
    )
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "the test helper mirrors the production descriptor-consuming configuration seam"
)]
#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "complete public test-model fixture construction"
)]
pub(crate) fn configured_model_for_operation(
    model: Arc<dyn LanguageModel>,
    descriptor: ResolvedModelDescriptor,
    authority: RuntimeModelAuthority,
    tools: impl IntoIterator<Item = ToolDefinition>,
    local_durability: LocalDurabilityDisposition,
    output_bound: OutputBoundDisposition,
    tool_mode: plakous_model::ToolPresentationMode,
) -> Result<ConfiguredLanguageModel, Box<dyn Error>> {
    let cache = if tool_mode == plakous_model::ToolPresentationMode::Direct
        && descriptor
            .capabilities()
            .get(Capability::Caching)
            .is_some_and(|fact| matches!(fact.support(), Support::Native | Support::Emulated))
    {
        CachePolicy::Enabled {
            requirement: CapabilityRequirement::Required,
        }
    } else {
        CachePolicy::Disabled
    };
    let behavior = ProviderBehaviorConfiguration {
        processing_tier: plakous_model::ProcessingTierDisposition::Standard,
        tool_mode,
        reasoning_effort: ReasoningEffortDisposition::Disabled,
        reasoning_summary: ReasoningSummaryDisposition::Omit,
        reasoning_include: ReasoningIncludeDisposition::Omit,
        replay: ReplayDisposition::Disabled,
        cache,
        tools: tools
            .into_iter()
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
            revision: ProviderProfileRevisionId::new("scripted-retention-v1")?,
        },
        response_state: ResponseStateDisposition::Stateless,
        context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
        local_durability,
    };
    let selected = SelectedProviderConfiguration::new(
        ProviderRouteSelection::new(
            descriptor.protocol().clone(),
            descriptor.route().clone(),
            descriptor.profile().clone(),
            ProviderProfileRevisionId::new("scripted-integration-v1")?,
            ModelAdapterProfileId::new("scripted.auth")?,
            ProviderProfileRevisionId::new("scripted-auth-v1")?,
            ProviderAuthorityClassId::new("scripted-test")?,
            CredentialSelectionId::new("scripted-test-credential")?,
            descriptor.requested_model().clone(),
        ),
        behavior.clone(),
    )?;
    let stable_authority = ResolvedProviderAuthorityBinding::new(
        &descriptor,
        &authority,
        ProviderSemanticEvidence::new(1, b"scripted-test-provider-semantics-v1".to_vec())?,
    )?;
    let family_policy = FamilyPolicyEnvelope::new(
        descriptor
            .model_family()
            .cloned()
            .ok_or("scripted test descriptor requires an explicit model family")?,
        1,
        1,
        1,
        b"scripted-test-family-policy-v1".to_vec(),
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
    ConfiguredLanguageModel::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        model,
        CompiledProviderConfiguration::new(selected, effective)?,
        authority,
    )
    .map_err(Into::into)
}
