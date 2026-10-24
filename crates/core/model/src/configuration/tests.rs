use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::{
    CapabilityConstraints, CapabilityRequirement, CapabilitySet, FactProvenance, FactSource,
    FeatureSupport, Timestamp, ToolConcurrency, ToolEffect, ToolInputContract, ToolName,
    ToolOutputContract,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;
type TestResult2 = Result<EffectiveProviderConfigurationId, Box<dyn std::error::Error>>;

struct StartCountingModel(AtomicUsize);

impl LanguageModel for StartCountingModel {
    fn prepare_call(
        self: Arc<Self>,
        _plan: crate::ValidatedRequestPlan,
        _context: crate::ModelPreparationContext,
    ) -> Result<crate::PreparedModelCall, crate::ModelPreparationError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("registration tests never prepare a call")
    }
}

fn route() -> Result<ProviderRouteSelection, crate::IdentityError> {
    Ok(ProviderRouteSelection::new(
        ProtocolId::new("responses.subscription")?,
        RouteId::new("subscription.responses")?,
        ModelAdapterProfileId::new("responses-profile")?,
        ProviderProfileRevisionId::new("adapter-v1")?,
        ModelAdapterProfileId::new("oauth-profile")?,
        ProviderProfileRevisionId::new("auth-v1")?,
        ProviderAuthorityClassId::new("oauth-subscription")?,
        CredentialSelectionId::new("primary")?,
        ModelId::new("exact-model")?,
    ))
}

fn retention() -> Result<RetentionDisposition, crate::IdentityError> {
    Ok(RetentionDisposition::RouteContract {
        profile: ModelAdapterProfileId::new("retention-profile")?,
        revision: ProviderProfileRevisionId::new("retention-v1")?,
    })
}

fn behavior() -> Result<ProviderBehaviorConfiguration, crate::IdentityError> {
    Ok(ProviderBehaviorConfiguration {
        processing_tier: ProcessingTierDisposition::Standard,
        tool_mode: ToolPresentationMode::Direct,
        reasoning_effort: ReasoningEffortDisposition::Disabled,
        reasoning_summary: ReasoningSummaryDisposition::Omit,
        reasoning_include: ReasoningIncludeDisposition::Omit,
        replay: ReplayDisposition::Disabled,
        cache: CachePolicy::Disabled,
        tools: Vec::new(),
        hosted_tools: Vec::new(),
        tool_choice: ToolChoice::None,
        parallel_tools: ParallelToolDisposition::Disabled,
        output: OutputPolicy::default(),
        output_bound: OutputBoundDisposition::NoRequestBound,
        storage: ProviderStorageDisposition::OmittedRouteContract,
        retention: retention()?,
        response_state: ResponseStateDisposition::Stateless,
        context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
        local_durability: LocalDurabilityDisposition::Ephemeral,
    })
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one matrix contrasts ordinary and workflow controls with every ceiling boundary"
)]
fn workflow_execution_controls_are_a_bounded_exact_refinement() -> TestResult {
    use crate::{
        AttemptId, ConversationView, GenerationLimits, ModelRequest, RequestMetadata, RunId,
        SessionWorkflowRootId, TurnId,
    };
    assert_eq!(SEMANTIC_COMPACTION_MAX_OUTPUT_TOKENS.get(), 16_384);
    let selected = SelectedProviderConfigurationId::new(Digest::sha256(
        DigestDomain::SelectedProviderConfiguration,
        5,
        &[b"controls-selected"],
    )?)?;
    let effective = EffectiveProviderConfigurationId::new(Digest::sha256(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        &[b"controls-effective"],
    )?)?;
    let run = RunId::try_from(uuid::Uuid::from_u128(1))?;
    let attempt = AttemptId::try_from(uuid::Uuid::from_u128(2))?;
    let time = Timestamp::new(1, 0)?;
    let turn = RequestMetadata::for_turn(
        run,
        TurnId::try_from(uuid::Uuid::from_u128(3))?,
        attempt,
        time,
    );
    let workflow = RequestMetadata::for_session_workflow(
        run,
        SessionWorkflowRootId::new([1; 32])?,
        attempt,
        time,
    );
    let mut configured = behavior()?;
    configured.tool_choice = ToolChoice::Auto;
    for (ceiling, metadata, choice, requested, stops, allowed) in [
        (Some(8192), turn, ToolChoice::Auto, Some(8192), false, true),
        (Some(8192), turn, ToolChoice::None, Some(4096), false, false),
        // The compaction bound is the ceiling, capped by the turn bound.
        (
            Some(32768),
            workflow,
            ToolChoice::None,
            Some(16384),
            false,
            true,
        ),
        (
            Some(8192),
            workflow,
            ToolChoice::None,
            Some(8192),
            false,
            true,
        ),
        (
            Some(4096),
            workflow,
            ToolChoice::None,
            Some(4096),
            false,
            true,
        ),
        (
            Some(32768),
            workflow,
            ToolChoice::None,
            Some(4096),
            false,
            false,
        ),
        (
            Some(32768),
            workflow,
            ToolChoice::None,
            Some(16383),
            false,
            false,
        ),
        (
            Some(32768),
            workflow,
            ToolChoice::None,
            Some(16385),
            false,
            false,
        ),
        (
            Some(8192),
            workflow,
            ToolChoice::None,
            Some(4096),
            false,
            false,
        ),
        (None, workflow, ToolChoice::None, Some(16384), false, false),
        (
            Some(32768),
            workflow,
            ToolChoice::Auto,
            Some(16384),
            false,
            false,
        ),
        (
            Some(32768),
            workflow,
            ToolChoice::None,
            Some(16384),
            true,
            false,
        ),
        (None, turn, ToolChoice::Auto, None, false, true),
    ] {
        configured.output_bound = ceiling.and_then(NonZeroU32::new).map_or(
            OutputBoundDisposition::NoRequestBound,
            OutputBoundDisposition::Enforced,
        );
        let request =
            ModelRequest::builder(selected, effective, ConversationView::default(), metadata)
                .tool_choice(choice)
                .limits(GenerationLimits::new(
                    requested.and_then(NonZeroU32::new),
                    if stops {
                        vec!["stop".to_owned()]
                    } else {
                        Vec::new()
                    },
                )?)
                .build()?;
        assert_eq!(
            configured.matches_request_execution_controls(&request),
            allowed,
            "ceiling={ceiling:?} requested={requested:?} root={:?}",
            metadata.root()
        );
    }
    // A manual thinking budget keeps the summary above it, still under
    // the turn bound; a 4,096-token budget no longer equals the bound.
    configured.output_bound =
        OutputBoundDisposition::Enforced(NonZeroU32::new(32_768).ok_or("ceiling")?);
    for (budget, requested, allowed) in [
        (4_096, 12_288, true),
        (20_000, 28_192, true),
        (30_000, 32_768, true),
        (4_096, 16_384, false),
        (4_096, 4_096, false),
    ] {
        let reasoning = ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::TokenBudget(
                NonZeroU32::new(budget).ok_or("budget")?,
            )),
            requirement: CapabilityRequirement::Required,
        };
        let request =
            ModelRequest::builder(selected, effective, ConversationView::default(), workflow)
                .tool_choice(ToolChoice::None)
                .reasoning(reasoning)
                .limits(GenerationLimits::new(
                    NonZeroU32::new(requested),
                    Vec::new(),
                )?)
                .build()?;
        assert_eq!(
            configured.matches_request_execution_controls(&request),
            allowed,
            "budget={budget} requested={requested}"
        );
    }
    Ok(())
}

fn descriptor() -> Result<ResolvedModelDescriptor, crate::IdentityError> {
    descriptor_at(1)
}

fn custom_descriptor() -> Result<ResolvedModelDescriptor, crate::IdentityError> {
    let descriptor = descriptor()?;
    Ok(ResolvedModelDescriptor::new_custom(
        descriptor.protocol().clone(),
        descriptor.route().clone(),
        descriptor.requested_model().clone(),
        descriptor.reported_model().cloned(),
        descriptor.model_family().cloned(),
        descriptor.profile().clone(),
        descriptor.capabilities().clone(),
    ))
}

fn descriptor_at(observed_at: u64) -> Result<ResolvedModelDescriptor, crate::IdentityError> {
    descriptor_for(
        "subscription.responses",
        "exact-model",
        Some("exact-model"),
        observed_at,
    )
}

fn descriptor_for(
    route: &str,
    requested_model: &str,
    reported_model: Option<&str>,
    observed_at: u64,
) -> Result<ResolvedModelDescriptor, crate::IdentityError> {
    let provenance = FactProvenance::new(
        FactSource::TestFixture,
        Timestamp::new(observed_at, 0)?,
        Some(Timestamp::new(observed_at + 100, 0)?),
    );
    let features = [
        Capability::Reasoning,
        Capability::Tools,
        Capability::ParallelTools,
        Capability::HostedCodeExecution,
        Capability::HostedProgrammaticToolCalling,
        Capability::StructuredOutput,
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
    .collect();
    Ok(ResolvedModelDescriptor::new(
        &StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("responses.subscription")?,
        RouteId::new(route)?,
        ModelId::new(requested_model)?,
        reported_model.map(ModelId::new).transpose()?,
        Some(ModelFamilyId::new("responses")?),
        ModelAdapterProfileId::new("responses-profile")?,
        CapabilitySet::new(features, BTreeMap::new()),
    ))
}

fn authority(
    descriptor: &ResolvedModelDescriptor,
) -> Result<RuntimeModelAuthority, Box<dyn std::error::Error>> {
    let account = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"])?;
    let credentials = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credentials"])?;
    Ok(RuntimeModelAuthority::new(
        descriptor,
        account,
        credentials,
        1,
        Timestamp::new(1, 0)?,
        Some(Timestamp::new(2, 0)?),
    )?)
}

fn stable_authority(
    descriptor: &ResolvedModelDescriptor,
) -> Result<ResolvedProviderAuthorityBinding, Box<dyn std::error::Error>> {
    Ok(ResolvedProviderAuthorityBinding::new(
        descriptor,
        &authority(descriptor)?,
        ProviderSemanticEvidence::new(1, b"fixture-semantic-evidence-v1".to_vec())?,
    )?)
}

fn family_policy(bytes: &[u8]) -> Result<FamilyPolicyEnvelope, Box<dyn std::error::Error>> {
    Ok(FamilyPolicyEnvelope::new(
        ModelFamilyId::new("responses")?,
        1,
        1,
        1,
        bytes.to_vec(),
    )?)
}

fn implementation() -> Result<ModelImplementationIdentity, Box<dyn std::error::Error>> {
    Ok(ModelImplementationIdentity::from_parts(
        crate::IntegrationClass::Standard,
        NamespaceId::new("plakous.test.scripted-model")?,
        1,
        NamespaceId::new("plakous.release")?,
    )?)
}

fn compiled(
    behavior: ProviderBehaviorConfiguration,
    policy: &[u8],
) -> Result<CompiledProviderConfiguration, Box<dyn std::error::Error>> {
    let selected = SelectedProviderConfiguration::new(route()?, behavior.clone())?;
    let descriptor = descriptor()?;
    let effective = EffectiveProviderConfiguration::compile(
        &selected,
        descriptor.clone(),
        implementation()?,
        stable_authority(&descriptor)?,
        behavior,
        family_policy(policy)?,
    )?;
    Ok(CompiledProviderConfiguration::new(selected, effective)?)
}

#[derive(Clone, Copy)]
struct EffectiveIdentityCase {
    route: &'static str,
    requested_model: &'static str,
    reported_model: &'static str,
    account: &'static [u8],
    credential_authority: &'static [u8],
    catalog_source_revision: u32,
    catalog_facts: &'static [u8],
    policy_schema_revision: u32,
    policy_compiler_revision: u32,
    policy_dialect_revision: u32,
    policy_facts: &'static [u8],
}

impl Default for EffectiveIdentityCase {
    fn default() -> Self {
        Self {
            route: "subscription.responses",
            requested_model: "exact-model",
            reported_model: "exact-model",
            account: b"account",
            credential_authority: b"credentials",
            catalog_source_revision: 1,
            catalog_facts: b"fixture-semantic-evidence-v1",
            policy_schema_revision: 1,
            policy_compiler_revision: 1,
            policy_dialect_revision: 1,
            policy_facts: b"policy-one",
        }
    }
}

fn compile_identity_case(
    case: EffectiveIdentityCase,
) -> Result<CompiledProviderConfiguration, Box<dyn std::error::Error>> {
    let behavior = behavior()?;
    let selected = SelectedProviderConfiguration::new(
        ProviderRouteSelection::new(
            ProtocolId::new("responses.subscription")?,
            RouteId::new(case.route)?,
            ModelAdapterProfileId::new("responses-profile")?,
            ProviderProfileRevisionId::new("adapter-v1")?,
            ModelAdapterProfileId::new("oauth-profile")?,
            ProviderProfileRevisionId::new("auth-v1")?,
            ProviderAuthorityClassId::new("oauth-subscription")?,
            CredentialSelectionId::new("primary")?,
            ModelId::new(case.requested_model)?,
        ),
        behavior.clone(),
    )?;
    let descriptor = descriptor_for(
        case.route,
        case.requested_model,
        Some(case.reported_model),
        1,
    )?;
    let runtime = RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[case.account])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[case.credential_authority])?,
        1,
        Timestamp::new(1, 0)?,
        Some(Timestamp::new(2, 0)?),
    )?;
    let stable = ResolvedProviderAuthorityBinding::new(
        &descriptor,
        &runtime,
        ProviderSemanticEvidence::new(case.catalog_source_revision, case.catalog_facts.to_vec())?,
    )?;
    let effective = EffectiveProviderConfiguration::compile(
        &selected,
        descriptor,
        implementation()?,
        stable,
        behavior,
        FamilyPolicyEnvelope::new(
            ModelFamilyId::new("responses")?,
            case.policy_schema_revision,
            case.policy_compiler_revision,
            case.policy_dialect_revision,
            case.policy_facts.to_vec(),
        )?,
    )?;
    Ok(CompiledProviderConfiguration::new(selected, effective)?)
}

#[test]
fn configuration_roundtrip_is_reconstruction_complete_and_deterministic() -> TestResult {
    let first = compiled(behavior()?, b"policy-one")?;
    let second = compiled(behavior()?, b"policy-one")?;
    assert_eq!(first, second);
    let bytes = serde_json::to_vec(&first)?;
    let decoded: CompiledProviderConfiguration = serde_json::from_slice(&bytes)?;
    assert_eq!(decoded, first);
    assert_eq!(
        decoded.selected().route().requested_model().as_str(),
        "exact-model"
    );
    assert_eq!(
        decoded.effective().family_policy().canonical_bytes(),
        b"policy-one"
    );
    Ok(())
}

fn configured_tool(
    description: &str,
    execution: ToolExecutionDisposition,
) -> Result<ConfiguredTool, Box<dyn std::error::Error>> {
    Ok(ConfiguredTool::new(
        ToolDefinition::new(
            ToolName::new("lookup")?,
            "1.0.0",
            description,
            ToolInputContract::structured(serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"],
                "additionalProperties": false
            }))?,
            ToolOutputContract::portable_model_content(),
            ToolConcurrency::Parallel,
            ToolEffect::ReadOnly,
        )?,
        execution,
        ToolCallerDisposition::DirectOnly,
    ))
}

/// A bound the product derives is identity-bearing like every other field,
/// and the two dispositions are distinct even at the same figure.
///
/// It has to be. The stored identity is recomputed from the stored
/// behaviour on every decode and compared to the stored digest, so any
/// convention that dropped the figure would make every store sealed under
/// the old one undecodable. Reopen therefore tolerates the movement by
/// substitution, not by digesting less.
#[test]
fn a_derived_bound_is_identity_bearing_and_distinct_from_a_chosen_one() -> TestResult {
    let bound = NonZeroU32::new(32_768).ok_or("bound")?;
    let far = NonZeroU32::new(200_000).ok_or("far")?;
    let identity = |disposition| -> TestResult2 {
        let mut value = behavior()?;
        value.output_bound = disposition;
        Ok(compiled(value, b"policy-one")?.effective_identity())
    };
    assert_ne!(
        identity(OutputBoundDisposition::DerivedFromRoute(bound))?,
        identity(OutputBoundDisposition::DerivedFromRoute(far))?,
        "the figure is digested, so a moved figure is a moved identity"
    );
    assert_ne!(
        identity(OutputBoundDisposition::Enforced(bound))?,
        identity(OutputBoundDisposition::DerivedFromRoute(bound))?,
        "the disposition is digested, so the two spellings differ"
    );
    assert!(OutputBoundDisposition::DerivedFromRoute(bound).is_derived_from_route());
    assert!(!OutputBoundDisposition::Enforced(bound).is_derived_from_route());
    assert_eq!(
        OutputBoundDisposition::DerivedFromRoute(bound).bound(),
        Some(bound)
    );
    assert_eq!(OutputBoundDisposition::NoRequestBound.bound(), None);
    Ok(())
}

/// Reopen asks whether everything but the derived figure still matches.
///
/// Three session shapes have to reopen on a route that derives its bound.
/// One created under a bound the product chose, before the route derived
/// anything; one created under the same derived figure the route still
/// attests; and one created under a derived figure that has since moved.
/// A configuration that differs in anything else reopens under none of
/// them, however the bound is substituted.
#[test]
fn substituting_the_persisted_bound_reopens_the_same_route_and_no_other() -> TestResult {
    let chosen_then = NonZeroU32::new(4_096).ok_or("chosen")?;
    let derived_now = NonZeroU32::new(32_768).ok_or("derived")?;

    let mut behavior_now = behavior()?;
    behavior_now.output_bound = OutputBoundDisposition::DerivedFromRoute(derived_now);
    let now = compiled(behavior_now, b"policy-one")?;
    let live = (now.selected_identity(), now.effective_identity());

    let persisted = |disposition| -> Result<
        (
            SelectedProviderConfigurationId,
            EffectiveProviderConfigurationId,
        ),
        Box<dyn std::error::Error>,
    > {
        let mut value = behavior()?;
        value.output_bound = disposition;
        let compiled = compiled(value, b"policy-one")?;
        Ok((compiled.selected_identity(), compiled.effective_identity()))
    };

    // Created under a chosen bound, before this route derived anything.
    let chosen = persisted(OutputBoundDisposition::Enforced(chosen_then))?;
    assert_ne!(chosen, live);
    assert_eq!(now.reopen_identities(Some(chosen_then), chosen)?, chosen);

    // Created under the figure the route still attests: nothing to move.
    let unmoved = persisted(OutputBoundDisposition::DerivedFromRoute(derived_now))?;
    assert_eq!(unmoved, live);
    assert_eq!(now.reopen_identities(Some(derived_now), unmoved)?, live);

    // Created under a derived figure the route has since moved.
    let moved_from = NonZeroU32::new(8_192).ok_or("moved")?;
    let moved = persisted(OutputBoundDisposition::DerivedFromRoute(moved_from))?;
    assert_ne!(moved, live);
    assert_eq!(now.reopen_identities(Some(moved_from), moved)?, moved);

    // A configuration that differs in something else never agrees, whatever
    // bound is substituted. `local_durability` stands for any of the fields
    // the identity covers.
    let mut other_behavior = behavior()?;
    other_behavior.output_bound = OutputBoundDisposition::DerivedFromRoute(derived_now);
    other_behavior.local_durability = LocalDurabilityDisposition::DurableRequired;
    let other = compiled(other_behavior, b"policy-one")?;
    assert_ne!(other.reopen_identities(Some(chosen_then), chosen)?, chosen);
    assert_ne!(other.reopen_identities(Some(moved_from), moved)?, moved);

    // And a different family policy, which stands for a different route.
    let mut foreign_behavior = behavior()?;
    foreign_behavior.output_bound = OutputBoundDisposition::DerivedFromRoute(derived_now);
    let foreign = compiled(foreign_behavior, b"policy-two")?;
    assert_ne!(
        foreign.reopen_identities(Some(chosen_then), chosen)?,
        chosen
    );

    // A route whose bound the product chose keeps the exact comparison, so
    // a changed bound there is still drift.
    let mut chosen_live = behavior()?;
    chosen_live.output_bound = OutputBoundDisposition::Enforced(derived_now);
    let chosen_live = compiled(chosen_live, b"policy-one")?;
    assert_ne!(
        chosen_live.reopen_identities(Some(chosen_then), chosen)?,
        chosen
    );
    Ok(())
}

#[test]
fn every_behavior_field_changes_selected_and_effective_identity() -> TestResult {
    let baseline = compiled(behavior()?, b"policy-one")?;
    let mut variants: Vec<(&str, ProviderBehaviorConfiguration)> = Vec::new();
    let mut value = behavior()?;
    value.processing_tier =
        ProcessingTierDisposition::ProviderNamed(ProviderServiceTierId::new("priority")?);
    variants.push(("processing_tier", value));
    let mut value = behavior()?;
    value.reasoning_effort = ReasoningEffortDisposition::Enabled(ReasoningEffort::Low);
    variants.push(("reasoning_effort", value));
    let mut value = behavior()?;
    value.reasoning_summary = ReasoningSummaryDisposition::Concise;
    variants.push(("reasoning_summary", value));
    let mut value = behavior()?;
    value.reasoning_include = ReasoningIncludeDisposition::EncryptedContent;
    variants.push(("reasoning_include", value));
    let mut value = behavior()?;
    value.replay = ReplayDisposition::DropUnreplayableReasoning;
    variants.push(("replay", value));
    let mut value = behavior()?;
    value.cache = CachePolicy::Enabled {
        requirement: CapabilityRequirement::Required,
    };
    variants.push(("cache", value));
    let mut value = behavior()?;
    value.tools = vec![configured_tool(
        "lookup definition one",
        ToolExecutionDisposition::Local,
    )?];
    variants.push(("tool_definition", value));
    let mut value = behavior()?;
    value.tools = vec![configured_tool(
        "lookup definition",
        ToolExecutionDisposition::ProviderHosted,
    )?];
    variants.push(("tool_owner", value));
    let mut value = behavior()?;
    value.tools = vec![configured_tool(
        "lookup definition",
        ToolExecutionDisposition::Local,
    )?];
    value.tool_choice = ToolChoice::Specific(ToolName::new("lookup")?);
    variants.push(("tool_choice", value));
    let mut value = behavior()?;
    value.tools = vec![configured_tool(
        "lookup definition",
        ToolExecutionDisposition::Local,
    )?];
    value.parallel_tools = ParallelToolDisposition::Enabled;
    variants.push(("parallel_tools", value));
    let mut value = behavior()?;
    value.output = OutputPolicy::new(
        OutputFormat::structured(serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"answer": {"type": "string"}},
            "required": ["answer"],
            "additionalProperties": false
        }))?,
        CapabilityRequirement::Required,
    )?;
    variants.push(("output_format", value));
    let mut value = behavior()?;
    value.output_bound = OutputBoundDisposition::Enforced(NonZeroU32::MIN);
    variants.push(("output_bound", value));
    let mut value = behavior()?;
    value.storage = ProviderStorageDisposition::DisabledOnWire;
    variants.push(("storage", value));
    let mut value = behavior()?;
    value.retention = RetentionDisposition::RouteContract {
        profile: ModelAdapterProfileId::new("retention-profile")?,
        revision: ProviderProfileRevisionId::new("retention-v2")?,
    };
    variants.push(("retention", value));
    let mut value = behavior()?;
    value.response_state = ResponseStateDisposition::ProviderStored;
    variants.push(("response_state", value));
    let mut value = behavior()?;
    value.context_evidence_requirement = ContextAdmissionRequirement::ReleaseVerifiedRequired;
    variants.push(("context_evidence_requirement", value));
    let mut value = behavior()?;
    value.local_durability = LocalDurabilityDisposition::DurableRequired;
    variants.push(("local_durability", value));
    for (field, variant) in variants {
        let changed = compiled(variant, b"policy-one")?;
        assert_ne!(
            changed.selected_identity(),
            baseline.selected_identity(),
            "{field}"
        );
        assert_ne!(
            changed.effective_identity(),
            baseline.effective_identity(),
            "{field}"
        );
    }
    Ok(())
}

#[test]
fn every_resolved_authority_and_policy_field_changes_exact_effective_identity() -> TestResult {
    let baseline_case = EffectiveIdentityCase::default();
    let baseline = compile_identity_case(baseline_case)?;
    let variants = [
        (
            "route",
            EffectiveIdentityCase {
                route: "subscription.responses.v2",
                ..baseline_case
            },
        ),
        (
            "requested_model",
            EffectiveIdentityCase {
                requested_model: "requested-model-v2",
                ..baseline_case
            },
        ),
        (
            "effective_model",
            EffectiveIdentityCase {
                reported_model: "effective-model-v2",
                ..baseline_case
            },
        ),
        (
            "account",
            EffectiveIdentityCase {
                account: b"account-v2",
                ..baseline_case
            },
        ),
        (
            "credential_authority",
            EffectiveIdentityCase {
                credential_authority: b"credentials-v2",
                ..baseline_case
            },
        ),
        (
            "catalog_modality_fact",
            EffectiveIdentityCase {
                catalog_facts: b"catalog-input-modalities-text-image",
                ..baseline_case
            },
        ),
        (
            "catalog_source_revision",
            EffectiveIdentityCase {
                catalog_source_revision: 2,
                ..baseline_case
            },
        ),
        (
            "policy_fact",
            EffectiveIdentityCase {
                policy_facts: b"policy-two",
                ..baseline_case
            },
        ),
        (
            "policy_schema_revision",
            EffectiveIdentityCase {
                policy_schema_revision: 2,
                ..baseline_case
            },
        ),
        (
            "policy_compiler_revision",
            EffectiveIdentityCase {
                policy_compiler_revision: 2,
                ..baseline_case
            },
        ),
        (
            "policy_dialect_revision",
            EffectiveIdentityCase {
                policy_dialect_revision: 2,
                ..baseline_case
            },
        ),
    ];
    for (field, variant) in variants {
        let changed = compile_identity_case(variant)?;
        assert_ne!(
            changed.effective_identity(),
            baseline.effective_identity(),
            "{field}"
        );
    }
    Ok(())
}

#[test]
fn selected_and_effective_behavior_cannot_disagree() -> TestResult {
    let selected_behavior = behavior()?;
    let selected = SelectedProviderConfiguration::new(route()?, selected_behavior.clone())?;
    let descriptor = descriptor()?;
    let mut different = selected_behavior;
    different.local_durability = LocalDurabilityDisposition::DurableRequired;
    assert_eq!(
        EffectiveProviderConfiguration::compile(
            &selected,
            descriptor.clone(),
            implementation()?,
            stable_authority(&descriptor)?,
            different,
            family_policy(b"policy-one")?,
        ),
        Err(ProviderConfigurationError::UnsupportedBehavior)
    );
    Ok(())
}

#[test]
fn tool_presentation_mode_rotates_selected_and_effective_identity() -> TestResult {
    let direct = behavior()?;
    let selected_direct = SelectedProviderConfiguration::new(route()?, direct.clone())?;
    let effective_direct = compile_identity_case(EffectiveIdentityCase::default())?;

    let mut composition_only = direct;
    composition_only.tool_mode = ToolPresentationMode::LocalCompositionOnly;
    let selected_composition =
        SelectedProviderConfiguration::new(route()?, composition_only.clone())?;
    let descriptor = descriptor()?;
    let effective_composition = EffectiveProviderConfiguration::compile(
        &selected_composition,
        descriptor.clone(),
        implementation()?,
        stable_authority(&descriptor)?,
        composition_only,
        family_policy(b"policy-one")?,
    )?;

    assert_ne!(selected_direct.identity(), selected_composition.identity());
    assert_ne!(
        effective_direct.effective_identity(),
        effective_composition.identity()
    );
    let round_trip: SelectedProviderConfiguration =
        serde_json::from_slice(&serde_json::to_vec(&selected_composition)?)?;
    assert_eq!(
        round_trip.behavior().tool_mode,
        ToolPresentationMode::LocalCompositionOnly
    );
    Ok(())
}

#[test]
fn hosted_selection_and_per_tool_caller_disposition_are_exact_configuration_facts() -> TestResult {
    let direct = behavior()?;
    let direct_selected = SelectedProviderConfiguration::new(route()?, direct.clone())?;

    let mut programmatic = behavior()?;
    programmatic.tools = vec![ConfiguredTool::new(
        configured_tool("lookup", ToolExecutionDisposition::Local)?
            .definition()
            .clone(),
        ToolExecutionDisposition::Local,
        ToolCallerDisposition::ProviderProgrammaticOnly,
    )];
    programmatic.hosted_tools = vec![HostedTool::ProgrammaticComposition];
    programmatic.tool_choice = ToolChoice::Auto;
    let programmatic_selected = SelectedProviderConfiguration::new(route()?, programmatic.clone())?;
    assert_ne!(direct_selected.identity(), programmatic_selected.identity());

    let mut both = programmatic.clone();
    both.tools[0] = ConfiguredTool::new(
        both.tools[0].definition().clone(),
        both.tools[0].execution(),
        ToolCallerDisposition::DirectAndProviderProgrammatic,
    );
    let both_selected = SelectedProviderConfiguration::new(route()?, both)?;
    assert_ne!(programmatic_selected.identity(), both_selected.identity());

    let descriptor = descriptor()?;
    let effective = EffectiveProviderConfiguration::compile(
        &programmatic_selected,
        descriptor.clone(),
        implementation()?,
        stable_authority(&descriptor)?,
        programmatic,
        family_policy(b"hosted-policy")?,
    )?;
    assert_eq!(
        effective.behavior().hosted_tools,
        vec![HostedTool::ProgrammaticComposition]
    );
    let round_trip: EffectiveProviderConfiguration =
        serde_json::from_slice(&serde_json::to_vec(&effective)?)?;
    assert_eq!(round_trip, effective);
    Ok(())
}

#[test]
fn tool_owner_projection_matrix_is_exact_and_does_not_coerce_ownership() -> TestResult {
    let definition = configured_tool("lookup", ToolExecutionDisposition::Local)?
        .definition()
        .clone();
    let direct = ConfiguredTool::new(
        definition.clone(),
        ToolExecutionDisposition::Local,
        ToolCallerDisposition::DirectOnly,
    );
    let programmatic = ConfiguredTool::new(
        definition.clone(),
        ToolExecutionDisposition::Local,
        ToolCallerDisposition::DirectAndProviderProgrammatic,
    );
    let mcp = ToolExecutionOwner::Mcp {
        server: McpServerId::new("plak-mcp")?,
    };
    assert_eq!(direct.validate_owner_projection(&mcp), Ok(()));
    assert_eq!(
        programmatic.validate_owner_projection(&ToolExecutionOwner::Local),
        Ok(())
    );
    assert_eq!(programmatic.validate_owner_projection(&mcp), Ok(()));

    let hosted = ConfiguredTool::new(
        definition,
        ToolExecutionDisposition::ProviderHosted,
        ToolCallerDisposition::DirectOnly,
    );
    let family_owner = ToolExecutionOwner::ProviderHosted {
        provider_kind: NamespaceId::new("family.hosted")?,
    };
    assert_eq!(hosted.validate_owner_projection(&family_owner), Ok(()));
    assert_eq!(
        hosted.validate_owner_projection(&ToolExecutionOwner::Local),
        Err(ToolOwnerProjectionError::OwnerMismatch)
    );
    assert_eq!(
        direct.validate_owner_projection(&family_owner),
        Err(ToolOwnerProjectionError::OwnerMismatch)
    );
    Ok(())
}

#[test]
fn an_mcp_owner_is_absent_from_every_other_tool_and_exact_when_present() -> TestResult {
    let local = configured_tool("lookup", ToolExecutionDisposition::Local)?;
    let encoded = serde_json::to_value(&local)?;
    let keys = encoded
        .as_object()
        .map(|object| object.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    assert_eq!(keys, ["caller", "definition", "execution"]);
    assert_eq!(local.local_owner(), Some(ToolExecutionOwner::Local));

    let mut plain_behavior = behavior()?;
    plain_behavior.tools = vec![local.clone()];
    let plain = compiled(plain_behavior.clone(), b"policy-one")?;
    let plain_bytes = serde_json::to_vec(&plain)?;
    assert!(!String::from_utf8(plain_bytes.clone())?.contains("mcp_server"));
    let decoded: CompiledProviderConfiguration = serde_json::from_slice(&plain_bytes)?;
    assert_eq!(serde_json::to_vec(&decoded)?, plain_bytes);
    assert_eq!(decoded.effective().identity(), plain.effective().identity());

    let server = McpServerId::new("github")?;
    let owned = local.clone().with_mcp_owner(server.clone())?;
    let mcp_owner = ToolExecutionOwner::Mcp {
        server: server.clone(),
    };
    assert_eq!(owned.local_owner(), Some(mcp_owner.clone()));
    assert_eq!(owned.validate_owner_projection(&mcp_owner), Ok(()));
    for wrong in [
        ToolExecutionOwner::Local,
        ToolExecutionOwner::Mcp {
            server: McpServerId::new("linear")?,
        },
    ] {
        assert_eq!(
            owned.validate_owner_projection(&wrong),
            Err(ToolOwnerProjectionError::OwnerMismatch)
        );
    }

    let mut owned_behavior = plain_behavior;
    owned_behavior.tools = vec![owned];
    let mcp = compiled(owned_behavior, b"policy-one")?;
    assert_ne!(mcp.selected().identity(), plain.selected().identity());
    assert_ne!(mcp.effective().identity(), plain.effective().identity());
    let mcp_bytes = serde_json::to_vec(&mcp)?;
    assert!(String::from_utf8(mcp_bytes.clone())?.contains("\"mcp_server\":\"github\""));
    let decoded: CompiledProviderConfiguration = serde_json::from_slice(&mcp_bytes)?;
    assert_eq!(decoded, mcp);

    let hosted = configured_tool("lookup", ToolExecutionDisposition::ProviderHosted)?;
    assert_eq!(hosted.local_owner(), None);
    assert_eq!(
        hosted.with_mcp_owner(server).err(),
        Some(ToolOwnerProjectionError::OwnerMismatch)
    );
    Ok(())
}

#[test]
fn hosted_configuration_fails_closed_on_shape_capability_and_compaction() -> TestResult {
    let definition = configured_tool("lookup", ToolExecutionDisposition::Local)?
        .definition()
        .clone();
    let mut missing_selection = behavior()?;
    missing_selection.tools = vec![ConfiguredTool::new(
        definition.clone(),
        ToolExecutionDisposition::Local,
        ToolCallerDisposition::ProviderProgrammaticOnly,
    )];
    missing_selection.tool_choice = ToolChoice::Auto;
    assert!(SelectedProviderConfiguration::new(route()?, missing_selection).is_err());

    let mut no_eligible_tool = behavior()?;
    no_eligible_tool.tools = vec![ConfiguredTool::new(
        definition,
        ToolExecutionDisposition::Local,
        ToolCallerDisposition::DirectOnly,
    )];
    no_eligible_tool.hosted_tools = vec![HostedTool::ProgrammaticComposition];
    no_eligible_tool.tool_choice = ToolChoice::Auto;
    assert!(SelectedProviderConfiguration::new(route()?, no_eligible_tool).is_err());

    let mut interpreter = behavior()?;
    interpreter.hosted_tools = vec![HostedTool::CodeInterpreter];
    let selected = SelectedProviderConfiguration::new(route()?, interpreter.clone())?;
    let supported = descriptor()?;
    let mut features = supported.capabilities().features().clone();
    features.remove(&Capability::HostedCodeExecution);
    let unsupported = ResolvedModelDescriptor::new(
        &StandardIntegrationAuthority::release_owned(),
        supported.protocol().clone(),
        supported.route().clone(),
        supported.requested_model().clone(),
        supported.reported_model().cloned(),
        supported.model_family().cloned(),
        supported.profile().clone(),
        CapabilitySet::new(features, BTreeMap::new()),
    );
    assert!(matches!(
        EffectiveProviderConfiguration::compile(
            &selected,
            unsupported.clone(),
            implementation()?,
            stable_authority(&unsupported)?,
            interpreter,
            family_policy(b"hosted-policy")?,
        ),
        Err(ProviderConfigurationError::UnsupportedBehavior)
    ));

    let mut hosted = behavior()?;
    hosted.hosted_tools = vec![HostedTool::CodeInterpreter];
    let compacted = compiled(hosted, b"hosted-policy")?.semantic_compaction()?;
    assert_eq!(
        compacted.selected().behavior().tools,
        [] as [ConfiguredTool; 0]
    );
    assert_eq!(
        compacted.selected().behavior().hosted_tools,
        [] as [HostedTool; 0]
    );
    Ok(())
}

#[test]
fn semantic_compaction_preserves_exact_processing_tier() -> TestResult {
    let mut priority = behavior()?;
    priority.processing_tier =
        ProcessingTierDisposition::ProviderNamed(ProviderServiceTierId::new("priority")?);

    let compacted = compiled(priority, b"priority-compaction")?.semantic_compaction()?;

    assert_eq!(
        compacted.selected().behavior().processing_tier,
        ProcessingTierDisposition::ProviderNamed(ProviderServiceTierId::new("priority")?)
    );
    assert_eq!(
        compacted.effective().behavior().processing_tier,
        ProcessingTierDisposition::ProviderNamed(ProviderServiceTierId::new("priority")?)
    );
    Ok(())
}

#[test]
fn missing_tampered_and_cross_bound_values_fail_closed() -> TestResult {
    let value = compiled(behavior()?, b"policy-one")?;
    let mut missing = serde_json::to_value(&value)?;
    missing["selected"]
        .as_object_mut()
        .ok_or("selected fixture must be an object")?
        .remove("behavior");
    assert!(serde_json::from_value::<CompiledProviderConfiguration>(missing).is_err());

    let mut tampered = serde_json::to_value(&value)?;
    tampered["effective"]["family_policy"]["canonical_bytes"] = serde_json::json!([9]);
    assert!(serde_json::from_value::<CompiledProviderConfiguration>(tampered).is_err());

    let other = SelectedProviderConfiguration::new(
        ProviderRouteSelection::new(
            ProtocolId::new("responses.subscription")?,
            RouteId::new("subscription.responses")?,
            ModelAdapterProfileId::new("responses-profile")?,
            ProviderProfileRevisionId::new("adapter-v2")?,
            ModelAdapterProfileId::new("oauth-profile")?,
            ProviderProfileRevisionId::new("auth-v1")?,
            ProviderAuthorityClassId::new("oauth-subscription")?,
            CredentialSelectionId::new("primary")?,
            ModelId::new("exact-model")?,
        ),
        behavior()?,
    )?;
    assert_eq!(
        CompiledProviderConfiguration::new(other, value.effective().clone()),
        Err(ProviderConfigurationError::ForeignSelection)
    );
    Ok(())
}

#[test]
fn generic_capability_mismatch_is_rejected() -> TestResult {
    let selected_behavior = ProviderBehaviorConfiguration {
        tool_mode: ToolPresentationMode::Direct,
        cache: CachePolicy::Enabled {
            requirement: CapabilityRequirement::Required,
        },
        hosted_tools: Vec::new(),
        ..behavior()?
    };
    let selected = SelectedProviderConfiguration::new(route()?, selected_behavior.clone())?;
    let descriptor = ResolvedModelDescriptor::new(
        &StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("responses.subscription")?,
        RouteId::new("subscription.responses")?,
        ModelId::new("exact-model")?,
        Some(ModelId::new("exact-model")?),
        Some(ModelFamilyId::new("responses")?),
        ModelAdapterProfileId::new("responses-profile")?,
        CapabilitySet::default(),
    );
    assert_eq!(
        EffectiveProviderConfiguration::compile(
            &selected,
            descriptor.clone(),
            implementation()?,
            stable_authority(&descriptor)?,
            selected_behavior,
            family_policy(b"policy")?,
        ),
        Err(ProviderConfigurationError::UnsupportedBehavior)
    );
    Ok(())
}

#[test]
fn refresh_only_authority_changes_do_not_rotate_effective_semantics() -> TestResult {
    let selected_behavior = behavior()?;
    let selected = SelectedProviderConfiguration::new(route()?, selected_behavior.clone())?;
    let first_descriptor = descriptor_at(1)?;
    let second_descriptor = descriptor_at(10)?;
    assert_ne!(
        first_descriptor.binding_digest()?,
        second_descriptor.binding_digest()?
    );
    assert_eq!(
        first_descriptor.semantic_binding_digest()?,
        second_descriptor.semantic_binding_digest()?
    );
    let account = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"])?;
    let credentials = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credentials"])?;
    let first_runtime = RuntimeModelAuthority::new(
        &first_descriptor,
        account,
        credentials,
        1,
        Timestamp::new(1, 0)?,
        Some(Timestamp::new(101, 0)?),
    )?;
    let second_runtime = RuntimeModelAuthority::new(
        &second_descriptor,
        account,
        credentials,
        2,
        Timestamp::new(10, 0)?,
        Some(Timestamp::new(110, 0)?),
    )?;
    let semantic = ProviderSemanticEvidence::new(1, b"same-semantic-facts".to_vec())?;
    let first_authority =
        ResolvedProviderAuthorityBinding::new(&first_descriptor, &first_runtime, semantic.clone())?;
    let second_authority =
        ResolvedProviderAuthorityBinding::new(&second_descriptor, &second_runtime, semantic)?;
    let first = EffectiveProviderConfiguration::compile(
        &selected,
        first_descriptor,
        implementation()?,
        first_authority,
        selected_behavior.clone(),
        family_policy(b"policy")?,
    )?;
    let second = EffectiveProviderConfiguration::compile(
        &selected,
        second_descriptor,
        implementation()?,
        second_authority,
        selected_behavior.clone(),
        family_policy(b"policy")?,
    )?;
    assert_eq!(first.identity(), second.identity());

    let changed_authority = ResolvedProviderAuthorityBinding::new(
        first.descriptor(),
        &first_runtime,
        ProviderSemanticEvidence::new(1, b"changed-semantic-facts".to_vec())?,
    )?;
    let changed = EffectiveProviderConfiguration::compile(
        &selected,
        first.descriptor().clone(),
        implementation()?,
        changed_authority,
        selected_behavior,
        family_policy(b"policy")?,
    )?;
    assert_ne!(first.identity(), changed.identity());
    Ok(())
}

#[test]
fn custom_registration_owns_descriptor_executable_and_implementation() -> TestResult {
    let behavior = behavior()?;
    let selected = SelectedProviderConfiguration::new(route()?, behavior.clone())?;
    let descriptor = custom_descriptor()?;
    let runtime = authority(&descriptor)?;
    let implementation = ModelImplementationIdentity::custom(
        NamespaceId::new("consumer.custom-model")?,
        3,
        NamespaceId::new("consumer.fixture")?,
    )?;
    let effective = EffectiveProviderConfiguration::compile(
        &selected,
        descriptor.clone(),
        implementation.clone(),
        stable_authority(&descriptor)?,
        behavior,
        family_policy(b"custom-policy")?,
    )?;
    let configuration = CompiledProviderConfiguration::new(selected, effective)?;
    let starts = Arc::new(StartCountingModel(AtomicUsize::new(0)));
    let configured = ConfiguredLanguageModel::custom(starts.clone(), configuration, runtime)?;
    assert_eq!(configured.registered().descriptor(), &descriptor);
    assert_eq!(configured.registered().implementation(), &implementation);
    assert_eq!(starts.0.load(Ordering::SeqCst), 0);
    Ok(())
}

#[test]
fn custom_registration_rejects_standard_configuration_before_start() -> TestResult {
    let configuration = compiled(behavior()?, b"standard-policy")?;
    let descriptor = configuration.effective().descriptor().clone();
    let runtime = authority(&descriptor)?;
    let starts = Arc::new(StartCountingModel(AtomicUsize::new(0)));
    let error = ConfiguredLanguageModel::custom(starts.clone(), configuration, runtime)
        .err()
        .ok_or("standard configuration was accepted by the custom lane")?;
    assert_eq!(error, ProviderConfigurationError::ForeignIntegrationClass);
    assert_eq!(starts.0.load(Ordering::SeqCst), 0);
    Ok(())
}
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one matrix compares deserialization, compilation and both registration authorities for every evidence class"
)]
fn evidence_compatibility_matrix_keeps_registration_authority_separate() -> TestResult {
    use crate::{
        CapacityCertificate, CapacityRule, ContextContract, ContextEvidenceSource,
        CountCertificateKind, EvidenceFreshness, IntegrationClass, MeasurementCertificate,
        SafeLimit, StandardIntegrationAuthority,
    };
    let standard_authority = StandardIntegrationAuthority::release_owned();
    let starts = Arc::new(StartCountingModel(AtomicUsize::new(0)));
    let standard_descriptor = descriptor()?;
    let binding = authority(&standard_descriptor)?.authority_class_binding()?;
    let receipt = Digest::sha256(
        DigestDomain::ProviderSemanticEvidence,
        1,
        &[b"matrix-context-receipt"],
    )?;
    let freshness = EvidenceFreshness::new(Timestamp::new(1, 0)?, Some(Timestamp::new(100, 0)?))?;
    for integration in [IntegrationClass::Standard, IntegrationClass::Custom] {
        for evidence in 0..3 {
            let (capacity, source, count) = match evidence {
                0 => (
                    CapacityCertificate::new(
                        &standard_authority,
                        "matrix-capacity",
                        "v1",
                        ContextEvidenceSource::ReleaseCertificate,
                        CapacityRule::SharedWindow {
                            total: SafeLimit::Exact(100),
                        },
                        None,
                        freshness,
                    )?,
                    ContextEvidenceSource::ReleaseCertificate,
                    CountCertificateKind::ExactTokenizer,
                ),
                1 => (
                    CapacityCertificate::reference_client_envelope(
                        &standard_authority,
                        "matrix-capacity",
                        "v1",
                        CapacityRule::ProviderReservedInput {
                            reported_context_window: None,
                            maximum_projected_input: 100,
                            auto_compact_input: None,
                        },
                        binding,
                        freshness,
                        receipt,
                        "v1",
                    )?,
                    ContextEvidenceSource::AuthenticatedDiscovery,
                    CountCertificateKind::ReferenceClientEnvelopeUpperBound {
                        catalog_receipt: receipt,
                    },
                ),
                _ => (
                    CapacityCertificate::custom_envelope(
                        "matrix-capacity",
                        "v1",
                        CapacityRule::ProviderReservedInput {
                            reported_context_window: None,
                            maximum_projected_input: 100,
                            auto_compact_input: None,
                        },
                        binding,
                        freshness,
                        receipt,
                        "v1",
                    )?,
                    ContextEvidenceSource::CallerDeclaration,
                    CountCertificateKind::CustomEnvelopeUpperBound {
                        declaration: receipt,
                    },
                ),
            };
            let measurement = if evidence == 2 {
                MeasurementCertificate::new(
                    "matrix-count",
                    "v1",
                    "matrix-count",
                    "v1",
                    source,
                    count,
                    Some(binding),
                    freshness,
                )?
            } else {
                MeasurementCertificate::standard(
                    &standard_authority,
                    "matrix-count",
                    "v1",
                    "matrix-count",
                    "v1",
                    source,
                    count,
                    (evidence != 0).then_some(binding),
                    freshness,
                )?
            };
            let context = ContextContract::new(
                ProtocolId::new("responses-family")?,
                "v1",
                "v1",
                "v1",
                capacity,
                vec![measurement],
            )?;
            let descriptor = if integration == IntegrationClass::Standard {
                standard_descriptor.clone()
            } else {
                custom_descriptor()?
            }
            .with_context_contract(context);
            let allowed = integration == IntegrationClass::Standard || evidence == 2;
            let wire = serde_json::to_vec(&descriptor)?;
            let decoded = serde_json::from_slice::<ResolvedModelDescriptor>(&wire);
            assert_eq!(
                decoded.is_ok(),
                allowed,
                "descriptor {integration:?} evidence {evidence}"
            );
            let selected_implementation = if integration == IntegrationClass::Standard {
                implementation()?
            } else {
                ModelImplementationIdentity::custom(
                    NamespaceId::new("matrix.custom")?,
                    1,
                    NamespaceId::new("matrix.caller")?,
                )?
            };
            let registered = if integration == IntegrationClass::Standard {
                RegisteredLanguageModel::standard(
                    &standard_authority,
                    descriptor.clone(),
                    selected_implementation.clone(),
                    starts.clone(),
                )
            } else {
                RegisteredLanguageModel::custom(
                    descriptor.clone(),
                    selected_implementation.clone(),
                    starts.clone(),
                )
            };
            assert_eq!(
                registered.is_ok(),
                allowed,
                "registration {integration:?} evidence {evidence}"
            );
            let behavior = behavior()?;
            let selected = SelectedProviderConfiguration::new(route()?, behavior.clone())?;
            let effective = EffectiveProviderConfiguration::compile(
                &selected,
                descriptor.clone(),
                selected_implementation.clone(),
                stable_authority(&descriptor)?,
                behavior,
                family_policy(b"matrix-policy")?,
            );
            assert_eq!(
                effective.is_ok(),
                allowed,
                "compilation {integration:?} evidence {evidence}"
            );
            if integration == IntegrationClass::Standard {
                assert!(
                    RegisteredLanguageModel::custom(
                        descriptor,
                        selected_implementation,
                        starts.clone()
                    )
                    .is_err(),
                    "deserialization and a caller envelope cannot grant maintained registration"
                );
            } else if evidence == 2 {
                assert!(
                    RegisteredLanguageModel::custom(descriptor, implementation()?, starts.clone())
                        .is_err(),
                    "custom descriptor cannot take maintained implementation identity"
                );
            }
        }
    }
    assert_eq!(starts.0.load(Ordering::SeqCst), 0);
    Ok(())
}
