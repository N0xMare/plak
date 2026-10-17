//! Minimal offline agent loop: `plakous_test_support::ScriptedModel` + curated `plakous::agent` facade.
//!
//! Pins the repository scripted-model agent loop:
//! 1. Build a deterministic [`plakous_test_support::ScriptedModel`] scenario (no network).
//! 2. Construct an agent via [`plakous::agent::AgentBuilder`].
//! 3. Optionally attach content-free [`plakous::agent::TelemetryEmitter`].
//! 4. `prompt` → await `TurnResult` → `shutdown`.
//!
//! Identity sources are fixed so the scripted request matches the agent request;
//! test support binds the runtime-owned harness-plan identity at preparation.
//!
//! The same binary carries the Codex Code Mode enrollment refusal for a Custom
//! descriptor, so the facade's offline checks share one link.
//!
//! Run:
//! ```text
//! cargo nextest run -p plakous -E test(scripted_quickstart_completes_and_shuts_down)
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "offline integration test asserts construction inputs"
)]

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::Arc;

use plakous::agent::{AgentBuilder, AgentRequestPolicy, TelemetryEmitter};
use plakous::model::{
    Actor, AttemptId, BlockCompleted, BlockStart, BlockStarted, Capability, CapabilityConstraints,
    CapabilitySet, CapacityCertificate, CapacityRule, CompiledProviderConfiguration,
    ConfiguredLanguageModel, ContentBlock, ContextAdmissionRequirement, ContextContract,
    ContextEvidenceSource, ConversationItem, ConversationView, CountCertificateKind,
    CountObservation, CredentialSelectionId, Digest, DigestDomain, EffectiveProviderConfiguration,
    EntryId, EvidenceFreshness, FactProvenance, FactSource, FamilyPolicyEnvelope, FeatureSupport,
    FinishDetails, FinishReason, FixedClock, GenerationLimits, LocalDurabilityDisposition,
    MeasurementCertificate, ModelAdapterProfileId, ModelEvent, ModelFamilyId, ModelId,
    ModelImplementationIdentity, ModelRequest, ModelResponse, NamespaceId, OutputBoundDisposition,
    OutputLocation, OutputPolicy, ParallelToolDisposition, ProtocolId, Provenance,
    ProviderAuthorityClassId, ProviderBehaviorConfiguration, ProviderProfileRevisionId,
    ProviderRouteSelection, ProviderSemanticEvidence, ProviderStorageDisposition,
    ReasoningEffortDisposition, ReasoningIncludeDisposition, ReasoningSummaryDisposition,
    ReplayDisposition, RequestMetadata, ResolvedModelDescriptor, ResolvedProviderAuthorityBinding,
    ResponseStarted, ResponseStateDisposition, RetentionDisposition, RouteId, RunId,
    RuntimeModelAuthority, SelectedProviderConfiguration, SequenceIdGenerator, Support, TextBlock,
    TextDelta, TextPhase, Timestamp, ToolChoice, TurnId, ValidationMode,
};
use plakous::tools::{ToolRegistryBuilder, WorkspacePolicy};
use tempfile::tempdir;
use uuid::Uuid;

const PROMPT: &str = "hello";
const RESPONSE: &str = "world";
const MAX_OUTPUT_TOKENS: u32 = 128;

#[tokio::main]
async fn run_scripted_quickstart() {
    let descriptor = streaming_descriptor();
    let authority = runtime_authority(&descriptor);
    let configuration = scripted_configuration(&descriptor, &authority);
    let expected = expected_first_request(&configuration);
    let scenario = text_complete_scenario(&descriptor, expected, RESPONSE);
    let model = Arc::new(
        plakous_test_support::ScriptedModel::new(descriptor, [scenario])
            .with_runtime_authority(authority.clone())
            .with_context_observation(context_observation()),
    );
    let configured = ConfiguredLanguageModel::custom(model.clone(), configuration, authority)
        .expect("configured scripted model");

    let directory = tempdir().expect("temp workspace");
    let workspace =
        Arc::new(WorkspacePolicy::host(directory.path(), ".").expect("workspace policy"));
    let clock = Arc::new(FixedClock::new(Timestamp::new(1, 0).expect("timestamp")));
    let ids = Arc::new(
        SequenceIdGenerator::new((1_u128..=64).map(Uuid::from_u128)).expect("id sequence"),
    );

    // Telemetry is off unless an emitter is supplied. Disabled is the product
    // default; pass TelemetryEmitter::enabled(sink) for TUI status strips.
    let agent = AgentBuilder::new(
        configured,
        ToolRegistryBuilder::default().freeze(),
        workspace,
    )
    .request_policy(
        AgentRequestPolicy::default().with_generation_limits(
            GenerationLimits::new(NonZeroU32::new(MAX_OUTPUT_TOKENS), Vec::new())
                .expect("generation limits"),
        ),
    )
    .identity_sources(clock, ids)
    .telemetry(Arc::new(TelemetryEmitter::disabled()))
    .build()
    .expect("agent");

    let turn = agent.prompt(PROMPT).await.expect("prompt admitted");
    let result = turn.result().await.expect("turn completed");
    let text = result.response().expect("completed response").output_text();
    assert_eq!(text, RESPONSE);
    println!("scripted quickstart ok: prompt={PROMPT:?} response={text:?}");
    println!(
        "model_rounds={} remaining_scenarios={}",
        result.model_rounds(),
        model.remaining_scenarios()
    );

    agent.shutdown().await.expect("shutdown");
}

fn streaming_descriptor() -> ResolvedModelDescriptor {
    let provenance = FactProvenance::new(
        FactSource::TestFixture,
        Timestamp::new(1, 0).expect("timestamp"),
        None,
    );
    let features = BTreeMap::from([(
        Capability::Streaming,
        FeatureSupport::new(
            Support::Native,
            CapabilityConstraints::default(),
            provenance,
        ),
    )]);
    ResolvedModelDescriptor::new_custom(
        ProtocolId::new("example.protocol").expect("protocol"),
        RouteId::new("example.route").expect("route"),
        ModelId::new("example.model").expect("model"),
        None,
        Some(ModelFamilyId::new("example").expect("family")),
        ModelAdapterProfileId::new("example.profile").expect("profile"),
        CapabilitySet::new(features, BTreeMap::new()),
    )
    .with_context_contract(context_contract())
}

fn context_contract() -> ContextContract {
    let freshness = EvidenceFreshness::new(Timestamp::new(1, 0).expect("timestamp"), None)
        .expect("context freshness");
    let (account, credential) = scripted_authority_bindings();
    let authority_binding = RuntimeModelAuthority::authority_class_for(account, credential)
        .expect("authority class binding");
    let declaration = Digest::sha256(
        DigestDomain::ModelBinding,
        1,
        &[b"scripted complete-projection upper bound: 64 tokens"],
    )
    .expect("custom declaration");
    ContextContract::new(
        ProtocolId::new("example.protocol").expect("protocol family"),
        "scripted-accounting-v1",
        "scripted-projection-v1",
        "scripted-reservation-v1",
        CapacityCertificate::custom_envelope(
            "scripted-capacity",
            "scripted-capacity-v1",
            CapacityRule::ProviderReservedInput {
                reported_context_window: None,
                maximum_projected_input: 1_000_000,
                auto_compact_input: None,
            },
            authority_binding,
            freshness,
            declaration,
            "scripted-declaration-v1",
        )
        .expect("capacity certificate"),
        vec![
            MeasurementCertificate::new(
                "scripted-counter",
                "scripted-exact-v1",
                "scripted-complete-projection",
                "scripted-method-v1",
                ContextEvidenceSource::CallerDeclaration,
                CountCertificateKind::CustomEnvelopeUpperBound { declaration },
                Some(authority_binding),
                freshness,
            )
            .expect("measurement certificate"),
        ],
    )
    .expect("context contract")
}

fn context_observation() -> CountObservation {
    CountObservation::new("scripted-counter", "scripted-exact-v1", 64).expect("context observation")
}

fn runtime_authority(descriptor: &ResolvedModelDescriptor) -> RuntimeModelAuthority {
    let (account, credential) = scripted_authority_bindings();
    RuntimeModelAuthority::new(
        descriptor,
        account,
        credential,
        1,
        Timestamp::new(1, 0).expect("timestamp"),
        None,
    )
    .expect("runtime authority")
}

fn scripted_authority_bindings() -> (Digest, Digest) {
    let account = Digest::sha256(
        DigestDomain::ModelBinding,
        1,
        &[b"scripted-example-account"],
    )
    .expect("account binding");
    let credential = Digest::sha256(
        DigestDomain::ModelBinding,
        1,
        &[b"scripted-example-credential"],
    )
    .expect("credential binding");
    (account, credential)
}

fn scripted_configuration(
    descriptor: &ResolvedModelDescriptor,
    authority: &RuntimeModelAuthority,
) -> CompiledProviderConfiguration {
    let behavior = ProviderBehaviorConfiguration {
        tool_mode: plakous_model::ToolPresentationMode::Direct,
        reasoning_effort: ReasoningEffortDisposition::Disabled,
        reasoning_summary: ReasoningSummaryDisposition::Omit,
        reasoning_include: ReasoningIncludeDisposition::Omit,
        replay: ReplayDisposition::Disabled,
        cache: plakous::model::CachePolicy::Disabled,
        processing_tier: plakous::model::ProcessingTierDisposition::Standard,
        tools: Vec::new(),
        hosted_tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        parallel_tools: ParallelToolDisposition::Disabled,
        output: OutputPolicy::default(),
        output_bound: OutputBoundDisposition::Enforced(
            NonZeroU32::new(MAX_OUTPUT_TOKENS).expect("output token bound"),
        ),
        storage: ProviderStorageDisposition::OmittedRouteContract,
        retention: RetentionDisposition::RouteContract {
            profile: descriptor.profile().clone(),
            revision: ProviderProfileRevisionId::new("example-retention-v1").expect("revision"),
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
            ProviderProfileRevisionId::new("example-route-v1").expect("revision"),
            ModelAdapterProfileId::new("example.auth").expect("adapter"),
            ProviderProfileRevisionId::new("example-auth-v1").expect("revision"),
            ProviderAuthorityClassId::new("scripted-example").expect("authority class"),
            CredentialSelectionId::new("scripted-example-credential").expect("credential"),
            descriptor.requested_model().clone(),
        ),
        behavior.clone(),
    )
    .expect("selected configuration");
    let stable_authority = ResolvedProviderAuthorityBinding::new(
        descriptor,
        authority,
        ProviderSemanticEvidence::new(1, b"scripted-example-semantics-v1".to_vec())
            .expect("semantic evidence"),
    )
    .expect("stable authority");
    let family_policy = FamilyPolicyEnvelope::new(
        descriptor.model_family().cloned().expect("model family"),
        1,
        1,
        1,
        b"scripted-example-family-policy-v1".to_vec(),
    )
    .expect("family policy");
    let effective = EffectiveProviderConfiguration::compile(
        &selected,
        descriptor.clone(),
        ModelImplementationIdentity::custom(
            NamespaceId::new("example.scripted-model").expect("implementation namespace"),
            1,
            NamespaceId::new("example.quickstart").expect("provenance namespace"),
        )
        .expect("implementation identity"),
        stable_authority,
        behavior,
        family_policy,
    )
    .expect("effective configuration");
    CompiledProviderConfiguration::new(selected, effective).expect("compiled configuration")
}

/// Request shape the agent produces for the first prompt under the fixed id
/// sequence: AgentId(1), SessionId(2), BranchId(3), RunId(4) at build; TurnId(5),
/// user EntryId(6), AttemptId(7) at prompt.
fn expected_first_request(configuration: &CompiledProviderConfiguration) -> ModelRequest {
    let user = ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(6)).expect("entry"),
        Actor::User,
        vec![ContentBlock::Text(
            TextBlock::new(PROMPT, None).expect("text"),
        )],
        Provenance::UserInput,
    )
    .expect("user item");
    ModelRequest::builder(
        configuration.selected_identity(),
        configuration.effective_identity(),
        ConversationView::from_items(vec![user])
            .expect("test conversation items form one valid segment"),
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(4)).expect("run"),
            TurnId::try_from(Uuid::from_u128(5)).expect("turn"),
            AttemptId::try_from(Uuid::from_u128(7)).expect("attempt"),
            Timestamp::new(1, 0).expect("timestamp"),
        ),
    )
    .limits(
        GenerationLimits::new(NonZeroU32::new(MAX_OUTPUT_TOKENS), Vec::new())
            .expect("generation limits"),
    )
    .build()
    .expect("request")
}

fn text_complete_scenario(
    descriptor: &ResolvedModelDescriptor,
    request: ModelRequest,
    text: &str,
) -> plakous_test_support::ModelScenario {
    let plan = descriptor
        .validate_request(request.clone(), ValidationMode::Strict)
        .expect("validate");
    let digest = plan.digest();
    let item = ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(100)).expect("assistant entry"),
        Actor::Assistant,
        vec![ContentBlock::Text(
            TextBlock::new(text, Some(TextPhase::Final)).expect("assistant text"),
        )],
        Provenance::ModelOutput,
    )
    .expect("assistant item");
    let location = OutputLocation::new(0, 0);
    let response = ModelResponse::new(
        vec![item.clone()],
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        digest,
    );
    plakous_test_support::ModelScenario::new(
        plakous_test_support::ExpectedRequest::ExactBeforeRuntimeHarnessPlan(Box::new(request)),
        plakous_test_support::ScenarioStart::Immediate,
        vec![
            plakous_test_support::ScenarioStep::Emit(ModelEvent::ResponseStarted(
                ResponseStarted::new(None, digest),
            )),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::BlockStarted(BlockStarted::new(
                item.id(),
                location,
                BlockStart::Text {
                    phase: Some(TextPhase::Final),
                },
            ))),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::TextDelta(TextDelta::new(
                location,
                text.as_bytes(),
            ))),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::BlockCompleted(
                BlockCompleted::new(location),
            )),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::ResponseCompleted(response)),
        ],
    )
}

#[cfg(test)]
#[test]
fn scripted_quickstart_completes_and_shuts_down() {
    run_scripted_quickstart();
}

#[cfg(feature = "composition")]
#[path = "../examples/common/scripted.rs"]
mod scripted;

/// A Custom adapter cannot borrow the Codex enrollment vocabulary.
#[cfg(feature = "composition")]
mod custom_code_mode_enrollment {
    use super::scripted;
    use plakous::construction::provider_model_projection_identity;
    use plakous::model::{LocalDurabilityDisposition, ToolPresentationMode};
    use plakous::tools::{
        CodexMinimalLocalCodeModeV1, JobConfig, ToolRegistryBuilder, register_packs,
        standard_coding_packs_in_memory,
    };

    #[test]
    fn custom_descriptor_cannot_borrow_codex_code_mode_enrollment() -> scripted::Fallible<()> {
        let model = scripted::ScriptedModel::new(
            &scripted::Vocabulary {
                protocol: plakous_compose::CODEX_PROJECTION_PROTOCOL,
                route: plakous_compose::CODEX_PROJECTION_ROUTE,
                profile: plakous_compose::CODEX_PROJECTION_PROFILE,
                family: "responses-family",
                model: "gpt-5.6-luna",
            },
            Vec::new(),
        )?;
        let mut registry = ToolRegistryBuilder::default();
        register_packs(&standard_coding_packs_in_memory()?, &mut registry)?;
        CodexMinimalLocalCodeModeV1::register_markers_into(&mut registry)?;
        let registry = registry.freeze();
        let outer = registry
            .definitions()
            .iter()
            .filter(|tool| matches!(tool.name().as_str(), "exec" | "wait"))
            .cloned()
            .collect::<Vec<_>>();
        let configuration = model.provider_configuration(
            ToolPresentationMode::LocalCompositionOnly,
            LocalDurabilityDisposition::DurableRequired,
            &outer,
        )?;
        let descriptor = CodexMinimalLocalCodeModeV1::compile(
            JobConfig::default(),
            registry.clone(),
            provider_model_projection_identity(&configuration)?,
        )?;
        let error = plakous_compose::validate_codex_minimal_local_code_mode_candidate_v1(
            &descriptor,
            descriptor.outer_projection(),
            registry,
            &configuration,
        )
        .err()
        .ok_or("Custom descriptor incorrectly borrowed Codex enrollment")?;
        assert!(
            error
                .to_string()
                .contains("not an enrolled exact Codex Code Mode target")
        );
        Ok(())
    }
}
