use std::collections::BTreeMap;
use std::num::NonZeroU32;

use plakous_model::{
    Capability, CapabilityConstraints, CapabilityRequirement, CapabilitySet, CredentialSelectionId,
    Digest, DigestDomain, FactProvenance, FactSource, FeatureSupport, ModelAdapterProfileId,
    ModelFamilyId, ModelId, OutputPolicy, ProtocolId, ProviderAuthorityClassId,
    ProviderBehaviorConfiguration, ProviderProfileRevisionId, ProviderRouteSelection,
    ProviderSemanticEvidence, RouteId, RuntimeModelAuthority, Support, Timestamp,
};

use super::*;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn descriptor() -> TestResult<ResolvedModelDescriptor> {
    let native = FeatureSupport::new(
        Support::Native,
        CapabilityConstraints::new(None, Some("offline Messages fixture".to_owned())),
        FactProvenance::new(FactSource::TestFixture, Timestamp::new(1, 0)?, None),
    );
    Ok(ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("anthropic.messages")?,
        RouteId::new("anthropic.platform")?,
        ModelId::new("claude-offline")?,
        None,
        Some(ModelFamilyId::new("claude-offline-family")?),
        ModelAdapterProfileId::new("anthropic.messages.offline.v1")?,
        CapabilitySet::new(
            BTreeMap::from([
                (Capability::Streaming, native.clone()),
                (Capability::Tools, native),
            ]),
            BTreeMap::new(),
        ),
    ))
}

fn behavior() -> TestResult<ProviderBehaviorConfiguration> {
    Ok(ProviderBehaviorConfiguration {
        processing_tier: plakous_model::ProcessingTierDisposition::Standard,
        tool_mode: plakous_model::ToolPresentationMode::Direct,
        reasoning_effort: ReasoningEffortDisposition::Disabled,
        reasoning_summary: ReasoningSummaryDisposition::Omit,
        reasoning_include: ReasoningIncludeDisposition::Omit,
        replay: ReplayDisposition::StrictEncrypted,
        cache: CachePolicy::Disabled,
        tools: Vec::new(),
        hosted_tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        parallel_tools: ParallelToolDisposition::Disabled,
        output: OutputPolicy::default(),
        output_bound: OutputBoundDisposition::Enforced(
            NonZeroU32::new(8_192).ok_or("nonzero output bound")?,
        ),
        storage: ProviderStorageDisposition::OmittedRouteContract,
        retention: RetentionDisposition::RouteContract {
            profile: ModelAdapterProfileId::new("anthropic.retention.offline")?,
            revision: ProviderProfileRevisionId::new("anthropic.retention.offline.v1")?,
        },
        response_state: ResponseStateDisposition::Stateless,
        context_evidence_requirement: ContextAdmissionRequirement::ReleaseVerifiedRequired,
        local_durability: LocalDurabilityDisposition::Ephemeral,
    })
}

fn selected(behavior: ProviderBehaviorConfiguration) -> TestResult<SelectedProviderConfiguration> {
    Ok(SelectedProviderConfiguration::new(
        ProviderRouteSelection::new(
            ProtocolId::new("anthropic.messages")?,
            RouteId::new("anthropic.platform")?,
            ModelAdapterProfileId::new("anthropic.messages.offline.v1")?,
            ProviderProfileRevisionId::new("anthropic.messages.offline.2026-08-11.v1")?,
            ModelAdapterProfileId::new("anthropic.platform.api-key")?,
            ProviderProfileRevisionId::new("anthropic.platform.api-key.v1")?,
            ProviderAuthorityClassId::new("anthropic.platform.api-key")?,
            CredentialSelectionId::new("anthropic.platform.explicit-api-key")?,
            ModelId::new("claude-offline")?,
        ),
        behavior,
    )?)
}

fn authority(descriptor: &ResolvedModelDescriptor) -> TestResult<ResolvedProviderAuthorityBinding> {
    let account = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"])?;
    let credential = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credential"])?;
    let runtime = RuntimeModelAuthority::new(
        descriptor,
        account,
        credential,
        1,
        Timestamp::new(2, 0)?,
        None,
    )?;
    Ok(ResolvedProviderAuthorityBinding::new(
        descriptor,
        &runtime,
        ProviderSemanticEvidence::new(1, b"messages-offline-evidence-v1".to_vec())?,
    )?)
}

fn compile(
    behavior: ProviderBehaviorConfiguration,
) -> Result<CompiledProviderConfiguration, Box<dyn std::error::Error>> {
    let descriptor = descriptor()?;
    let authority = authority(&descriptor)?;
    Ok(compile_messages_configuration(
        &selected(behavior)?,
        descriptor,
        authority,
        "2023-06-01",
        "",
    )?)
}

#[test]
fn exact_offline_policy_is_deterministic_and_declares_no_aggregate_native_state() -> TestResult {
    let first = compile(behavior()?)?;
    let second = compile(behavior()?)?;
    assert_eq!(first, second);
    let policy: serde_json::Value =
        serde_json::from_slice(first.effective().family_policy().canonical_bytes())?;
    assert_eq!(policy["aggregate_continuation"], "absent");
    assert_eq!(policy["managed_cache_artifact"], "absent");
    assert_eq!(policy["native_compaction"], "absent");
    assert_eq!(
        policy["item_replay_schema_revision"],
        crate::MESSAGES_REPLAY_SCHEMA_REVISION
    );
    assert!(policy.get("artifact_schema").is_none());
    assert!(policy.get("artifact_codec").is_none());

    let mut parallel = behavior()?;
    parallel.tool_choice = ToolChoice::None;
    assert_ne!(
        first.effective().identity(),
        compile(parallel)?.effective().identity()
    );
    let mut durable = behavior()?;
    durable.local_durability = LocalDurabilityDisposition::DurableRequired;
    assert_ne!(
        first.effective().identity(),
        compile(durable)?.effective().identity()
    );
    Ok(())
}

#[test]
fn adaptive_api_admits_required_and_preferred_caching() -> TestResult {
    for requirement in [
        CapabilityRequirement::Required,
        CapabilityRequirement::Preferred,
    ] {
        let mut value = behavior()?;
        value.reasoning_effort =
            ReasoningEffortDisposition::Enabled(plakous_model::ReasoningEffort::Low);
        value.context_evidence_requirement = ContextAdmissionRequirement::AnyBounded;
        value.cache = CachePolicy::Enabled { requirement };
        validate_messages_api_behavior(&value)?;
    }
    Ok(())
}

#[test]
fn one_field_policy_drift_and_cache_writes_fail_before_dispatch() -> TestResult {
    let mut variants = Vec::new();
    let mut value = behavior()?;
    value.cache = CachePolicy::Enabled {
        requirement: CapabilityRequirement::Required,
    };
    variants.push(value);
    let mut value = behavior()?;
    value.replay = ReplayDisposition::Disabled;
    variants.push(value);
    let mut value = behavior()?;
    value.reasoning_include = ReasoningIncludeDisposition::EncryptedContent;
    variants.push(value);
    let mut value = behavior()?;
    value.storage = ProviderStorageDisposition::DisabledOnWire;
    variants.push(value);
    let mut value = behavior()?;
    value.response_state = ResponseStateDisposition::ProviderStored;
    variants.push(value);
    let mut value = behavior()?;
    value.context_evidence_requirement = ContextAdmissionRequirement::AnyBounded;
    variants.push(value);
    let mut value = behavior()?;
    value.output_bound = OutputBoundDisposition::NoRequestBound;
    variants.push(value);

    for variant in variants {
        assert!(matches!(
            compile(variant),
            Err(error) if error.downcast_ref::<MessagesProviderConfigurationError>()
                == Some(&MessagesProviderConfigurationError::PolicyMismatch)
        ));
    }
    assert!(matches!(
        compile_messages_configuration(
            &selected(behavior()?)?,
            descriptor()?,
            authority(&descriptor()?)?,
            "2023-06-01",
            "unsupported-beta",
        ),
        Err(MessagesProviderConfigurationError::PolicyMismatch)
    ));
    Ok(())
}
