use std::collections::BTreeMap;
use std::num::NonZeroU32;

use plakous_model::{
    CachePolicy, Capability, CapabilityConstraints, CapabilitySet, ContextAdmissionRequirement,
    ConversationView, CredentialSelectionId, Digest, DigestDomain, FactProvenance, FactSource,
    FeatureSupport, LocalDurabilityDisposition, ModelAdapterProfileId, ModelFamilyId, ModelId,
    ModelRequest, ModelRequestBuilder, OutputBoundDisposition, OutputPolicy,
    ParallelToolDisposition, ProviderAuthorityClassId, ProviderBehaviorConfiguration,
    ProviderProfileRevisionId, ProviderRouteSelection, ProviderSemanticEvidence,
    ProviderStorageDisposition, ReasoningEffortDisposition, ReasoningIncludeDisposition,
    ReasoningSummaryDisposition, ReplayDisposition, RequestMetadata, ResolvedModelDescriptor,
    ResolvedProviderAuthorityBinding, ResponseStateDisposition, RetentionDisposition, RouteId,
    RuntimeModelAuthority, SelectedProviderConfiguration, Support, Timestamp, ToolChoice,
};

use crate::{ResponsesRequestPolicy, compile_provider_configuration};

pub(crate) fn model_request_builder(
    input: ConversationView,
    metadata: RequestMetadata,
) -> ModelRequestBuilder {
    let integration_profile =
        ModelAdapterProfileId::new("responses-test-adapter").expect("integration profile fixture");
    let integration_revision =
        ProviderProfileRevisionId::new("responses-test-v1").expect("integration revision");
    let behavior = request_behavior(integration_profile.clone(), integration_revision.clone());
    let selected = SelectedProviderConfiguration::new(
        ProviderRouteSelection::new(
            plakous_model::ProtocolId::new("openai.responses").expect("protocol fixture"),
            RouteId::new("responses-test-route").expect("route fixture"),
            integration_profile.clone(),
            integration_revision,
            ModelAdapterProfileId::new("responses-test-auth").expect("auth profile fixture"),
            ProviderProfileRevisionId::new("responses-test-auth-v1")
                .expect("auth revision fixture"),
            ProviderAuthorityClassId::new("test-subscription").expect("authority class fixture"),
            CredentialSelectionId::new("test-account").expect("credential fixture"),
            ModelId::new("gpt-test").expect("model fixture"),
        ),
        behavior,
    )
    .expect("selected provider configuration fixture");
    let provenance = FactProvenance::new(
        FactSource::TestFixture,
        Timestamp::new(1_700_000_000, 0).expect("timestamp fixture"),
        None,
    );
    let capabilities = [
        Capability::Streaming,
        Capability::Tools,
        Capability::Continuation,
        Capability::Caching,
    ]
    .into_iter()
    .map(|capability| {
        (
            capability,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                provenance,
            ),
        )
    })
    .collect::<BTreeMap<_, _>>();
    let descriptor = ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        plakous_model::ProtocolId::new("openai.responses").expect("protocol fixture"),
        RouteId::new("responses-test-route").expect("route fixture"),
        ModelId::new("gpt-test").expect("model fixture"),
        None,
        Some(ModelFamilyId::new("gpt-family").expect("family fixture")),
        integration_profile,
        CapabilitySet::new(capabilities, BTreeMap::new()),
    );
    let runtime = RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"responses-test-account"])
            .expect("account binding fixture"),
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"responses-test-credentials"],
        )
        .expect("credential binding fixture"),
        1,
        Timestamp::new(1_700_000_000, 0).expect("observed timestamp fixture"),
        Some(Timestamp::new(1_700_000_100, 0).expect("expiry timestamp fixture")),
    )
    .expect("runtime authority fixture");
    let authority = ResolvedProviderAuthorityBinding::new(
        &descriptor,
        &runtime,
        ProviderSemanticEvidence::new(1, b"responses-test-semantic-v1".to_vec())
            .expect("semantic evidence fixture"),
    )
    .expect("provider authority fixture");
    let compiled = compile_provider_configuration(
        &selected,
        descriptor,
        authority,
        ResponsesRequestPolicy::supergrok_stateless(),
    )
    .expect("compiled provider configuration fixture");
    ModelRequest::builder(
        compiled.selected_identity(),
        compiled.effective_identity(),
        input,
        metadata,
    )
}

fn request_behavior(
    integration_profile: ModelAdapterProfileId,
    integration_revision: ProviderProfileRevisionId,
) -> ProviderBehaviorConfiguration {
    ProviderBehaviorConfiguration {
        processing_tier: plakous_model::ProcessingTierDisposition::Standard,
        tool_mode: plakous_model::ToolPresentationMode::Direct,
        reasoning_effort: ReasoningEffortDisposition::Disabled,
        reasoning_summary: ReasoningSummaryDisposition::Omit,
        reasoning_include: ReasoningIncludeDisposition::EncryptedContent,
        replay: ReplayDisposition::StrictEncrypted,
        cache: CachePolicy::Disabled,
        tools: Vec::new(),
        hosted_tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        parallel_tools: ParallelToolDisposition::Disabled,
        output: OutputPolicy::default(),
        output_bound: OutputBoundDisposition::Enforced(NonZeroU32::MIN),
        storage: ProviderStorageDisposition::DisabledOnWire,
        retention: RetentionDisposition::RouteContract {
            profile: integration_profile,
            revision: integration_revision,
        },
        response_state: ResponseStateDisposition::Stateless,
        context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
        local_durability: LocalDurabilityDisposition::Ephemeral,
    }
}
