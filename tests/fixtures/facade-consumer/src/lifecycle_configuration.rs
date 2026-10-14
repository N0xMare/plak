use super::{ConsumerModel, ResultBox};
use plakous::construction::CustomProviderConfigurationRequest;
use plakous::model::{
    CachePolicy, CompiledProviderConfiguration, ConfiguredTool, ContextAdmissionRequirement,
    CredentialSelectionId, LocalDurabilityDisposition, ModelAdapterProfileId,
    ModelImplementationIdentity, NamespaceId, OutputBoundDisposition, OutputPolicy,
    ParallelToolDisposition, ProcessingTierDisposition, ProviderAuthorityClassId,
    ProviderBehaviorConfiguration, ProviderProfileRevisionId, ProviderSemanticEvidence,
    ProviderStorageDisposition, ReasoningEffortDisposition, ReasoningIncludeDisposition,
    ReasoningSummaryDisposition, ReplayDisposition, ResponseStateDisposition, RetentionDisposition,
    ToolCallerDisposition, ToolChoice, ToolDefinition, ToolExecutionDisposition,
    ToolPresentationMode,
};

pub(super) fn provider_configuration(
    model: &ConsumerModel,
    tools: &[ToolDefinition],
) -> ResultBox<CompiledProviderConfiguration> {
    let descriptor = model.descriptor();
    let behavior = ProviderBehaviorConfiguration {
        processing_tier: ProcessingTierDisposition::Standard,
        tool_mode: ToolPresentationMode::Direct,
        reasoning_effort: ReasoningEffortDisposition::Disabled,
        reasoning_summary: ReasoningSummaryDisposition::Omit,
        reasoning_include: ReasoningIncludeDisposition::Omit,
        replay: ReplayDisposition::Disabled,
        cache: CachePolicy::Disabled,
        tools: tools
            .iter()
            .cloned()
            .map(|definition| {
                ConfiguredTool::new(
                    definition,
                    ToolExecutionDisposition::Local,
                    ToolCallerDisposition::DirectOnly,
                )
            })
            .collect(),
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
    };
    // Route, integration profile, requested model, and model family come off the
    // descriptor; only the issuer facts below belong to this consumer.
    Ok(CustomProviderConfigurationRequest {
        descriptor,
        authority: &model.authority,
        behavior,
        integration_revision: ProviderProfileRevisionId::new("consumer-route-v1")?,
        auth_profile: ModelAdapterProfileId::new("consumer.auth")?,
        auth_revision: ProviderProfileRevisionId::new("consumer-auth-v1")?,
        authority_class: ProviderAuthorityClassId::new("consumer-scripted")?,
        credential_selection: CredentialSelectionId::new("consumer-credential")?,
        implementation: ModelImplementationIdentity::custom(
            NamespaceId::new("consumer.model")?,
            1,
            NamespaceId::new("consumer.fixture")?,
        )?,
        semantic_evidence: ProviderSemanticEvidence::new(1, b"consumer-semantics-v1".to_vec())?,
        family_policy_schema_revision: 1,
        family_policy_compiler_revision: 1,
        family_policy_dialect_revision: 1,
        family_policy_bytes: b"consumer-family-policy-v1".to_vec(),
    }
    .compile()?)
}
