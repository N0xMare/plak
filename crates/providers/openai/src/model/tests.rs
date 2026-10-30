use std::collections::BTreeMap;
use std::error::Error;

use plakous_model::{
    ActiveFamilyArtifact, Actor, AttemptId, CallId, CanonicalHistory, Capability,
    CapabilityConstraints, CapabilitySet, ContentBlock, ConversationItem, ConversationView,
    DataClassification, Digest, DigestDomain, EffectiveProviderConfigurationId, EntryId,
    FactProvenance, FactSource, FamilyArtifactProducerFacts, FeatureSupport, FinishDetails,
    FinishReason, HistorySegment, HistorySegmentKind, ModelAdapterProfileId, ModelRequest,
    ModelResponse, OpaqueBlock, OpaqueFallback, ProtocolId, Provenance, ReplayEnvelope,
    ReplayKindId, ReplayScope, RequestMetadata, ResolvedModelDescriptor, RouteId, RunId,
    SealedFamilyArtifactCandidate, SelectedProviderConfigurationId, Support, TextBlock, TextPhase,
    Timestamp, ToolCallBlock, ToolCallState, ToolConcurrency, ToolCorrelation, ToolDefinition,
    ToolEffect, ToolExecutionOwner, ToolInputContract, ToolName, ToolOutputContract,
    ToolPresentationMode, ToolResultBlock, ToolResultKind, ToolResultPart, TurnId,
    ValidatedRequestPlan, ValidationMode, WorkingProjection,
};
use plakous_responses::{
    RESPONSES_ARTIFACT_CODEC_REVISION, RESPONSES_ARTIFACT_SCHEMA_REVISION, ResponsesArtifact,
    ResponsesArtifactBinding, ResponsesArtifactCodec, ResponsesArtifactProduction,
    ResponsesArtifactReplayKind, ResponsesReplayContract, ResponsesReplayItem,
    ResponsesReplayPurpose, ResponsesSemanticRevisions, ResponsesStorageMode,
};
use plakous_responses_dialect::{
    ResponsesReplayMode, ResponsesRequestAuthority, ResponsesRequestConfig, ResponsesStoragePolicy,
    lower_responses_request,
};
use serde_json::value::RawValue;

use super::{
    CODEX_COMPLETE_INPUT_BYTES_PER_TOKEN, codex_artifact_replay_bridge,
    codex_complete_input_token_upper_bound, codex_reference_client_input_tokens,
    codex_request_policy, codex_tool_mode_accepts,
};
use crate::{
    CodexCatalogToolMode, OPENAI_CODEX_COMPATIBILITY_PROFILE_ID, OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID,
};

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const NOW_SECONDS: u64 = 1_700_000_000;
const HARNESS_PLAN_TAG: &[u8] = b"codex-artifact-model-test-plan";

#[test]
fn catalog_tool_mode_admits_only_its_exact_tool_shape() {
    let controls = ["exec", "wait"];
    let neighboring = ["exec_command", "write_stdin"];
    assert!(codex_tool_mode_accepts(
        Some(CodexCatalogToolMode::Direct),
        ToolPresentationMode::Direct,
        &neighboring
    ));
    assert!(!codex_tool_mode_accepts(
        Some(CodexCatalogToolMode::Direct),
        ToolPresentationMode::LocalCompositionOnly,
        &controls
    ));
    assert!(!codex_tool_mode_accepts(
        Some(CodexCatalogToolMode::CodeMode),
        ToolPresentationMode::LocalComposition,
        &controls
    ));
    assert!(!codex_tool_mode_accepts(
        Some(CodexCatalogToolMode::CodeMode),
        ToolPresentationMode::LocalComposition,
        &neighboring
    ));
    assert!(codex_tool_mode_accepts(
        Some(CodexCatalogToolMode::CodeModeOnly),
        ToolPresentationMode::LocalCompositionOnly,
        &controls
    ));
    assert!(codex_tool_mode_accepts(
        Some(CodexCatalogToolMode::CodeModeOnly),
        ToolPresentationMode::Direct,
        &neighboring
    ));
    // Plak js/py-ptc replaces the explicit Direct presentation on a
    // CodeModeOnly row; the outer markers are not `exec`/`wait`.
    for plak_ptc in [&["exec_js", "wait_js"][..], &["exec_py", "wait_py"][..]] {
        assert!(codex_tool_mode_accepts(
            Some(CodexCatalogToolMode::CodeModeOnly),
            ToolPresentationMode::Direct,
            plak_ptc
        ));
        assert!(codex_tool_mode_accepts(
            Some(CodexCatalogToolMode::Direct),
            ToolPresentationMode::Direct,
            plak_ptc
        ));
    }
    assert!(!codex_tool_mode_accepts(
        None,
        ToolPresentationMode::Direct,
        &neighboring
    ));
    assert!(!codex_tool_mode_accepts(
        None,
        ToolPresentationMode::LocalCompositionOnly,
        &controls
    ));
    assert!(!codex_tool_mode_accepts(
        Some(CodexCatalogToolMode::CodeModeOnly),
        ToolPresentationMode::LocalCompositionOnly,
        &["wait", "neighboring-model-exec"]
    ));
    assert!(!codex_tool_mode_accepts(
        Some(CodexCatalogToolMode::Direct),
        ToolPresentationMode::Direct,
        &["exec", "exec_command"]
    ));
    assert!(!codex_tool_mode_accepts(
        None,
        ToolPresentationMode::Direct,
        &["wait"]
    ));
    assert!(!codex_tool_mode_accepts(
        Some(CodexCatalogToolMode::CodeMode),
        ToolPresentationMode::LocalComposition,
        &["exec", "exec_command"]
    ));
    assert!(codex_tool_mode_accepts(
        Some(CodexCatalogToolMode::CodeMode),
        ToolPresentationMode::LocalComposition,
        &["exec", "wait", "exec_command"]
    ));
    assert!(!codex_tool_mode_accepts(
        Some(CodexCatalogToolMode::CodeModeOnly),
        ToolPresentationMode::Direct,
        &controls
    ));
}

struct TransitionFixture {
    first: ModelRequest,
    second: ModelRequest,
    first_output: ConversationItem,
    candidate: SealedFamilyArtifactCandidate,
    active: ActiveFamilyArtifact,
}

fn now() -> TestResult<Timestamp> {
    Ok(Timestamp::new(NOW_SECONDS, 0)?)
}

fn descriptor(route: &str) -> TestResult<ResolvedModelDescriptor> {
    let observed = now()?;
    let provenance = FactProvenance::new(FactSource::TestFixture, observed, None);
    let mut capabilities = BTreeMap::new();
    capabilities.insert(
        Capability::Streaming,
        FeatureSupport::new(
            Support::Native,
            CapabilityConstraints::default(),
            provenance,
        ),
    );
    let requested = plakous_model::ModelId::new("gpt-codex-artifact-test")?;
    Ok(ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("openai.responses")?,
        RouteId::new(route)?,
        requested.clone(),
        Some(requested),
        None,
        ModelAdapterProfileId::new(OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)?,
        CapabilitySet::new(
            capabilities,
            BTreeMap::from([(
                plakous_model::NamespaceId::new("plakous.compaction")?,
                FeatureSupport::new(
                    Support::Native,
                    CapabilityConstraints::default(),
                    FactProvenance::new(FactSource::TestFixture, observed, None),
                ),
            )]),
        ),
    ))
}

fn metadata(attempt: &str) -> TestResult<RequestMetadata> {
    Ok(RequestMetadata::for_turn(
        "00000000-0000-7000-8000-000000000301".parse::<RunId>()?,
        "00000000-0000-7000-8000-000000000302".parse::<TurnId>()?,
        attempt.parse::<AttemptId>()?,
        now()?,
    ))
}

fn harness_plan_identity() -> TestResult<Digest> {
    Ok(Digest::sha256(
        DigestDomain::ResolvedHarnessPlan,
        1,
        &[HARNESS_PLAN_TAG],
    )?)
}

fn provider_configuration_ids() -> TestResult<(
    SelectedProviderConfigurationId,
    EffectiveProviderConfigurationId,
)> {
    Ok((
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::SelectedProviderConfiguration,
            5,
            &[b"openai-model-test-selected-configuration"],
        )?)?)?,
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"openai-model-test-effective-configuration"],
        )?)?)?,
    ))
}

fn request(segments: Vec<HistorySegment>, attempt: &str) -> TestResult<ModelRequest> {
    let (selected, effective) = provider_configuration_ids()?;
    Ok(ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_shared(
            CanonicalHistory::new(segments)?,
            WorkingProjection::default(),
        ),
        metadata(attempt)?,
    )
    .resolved_harness_plan_identity(harness_plan_identity()?)
    .build()?)
}

fn text_request(text: &str, attempt: &str, include_tool: bool) -> TestResult<ModelRequest> {
    let item = user("00000000-0000-7000-8000-000000000399", text)?;
    let (selected, effective) = provider_configuration_ids()?;
    let mut builder = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![item])?,
        metadata(attempt)?,
    )
    .resolved_harness_plan_identity(harness_plan_identity()?);
    if include_tool {
        builder = builder.tools(vec![plakous_model::ConfiguredTool::new(
            ToolDefinition::new(
                ToolName::new("lookup")?,
                "1.0.0",
                "Look up one Unicode value: 🦀",
                ToolInputContract::structured(serde_json::json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "properties": {"q": {"type": "string"}},
                    "required": ["q"],
                    "additionalProperties": false
                }))?,
                ToolOutputContract::portable_model_content(),
                ToolConcurrency::Serial,
                ToolEffect::ReadOnly,
            )?,
            plakous_model::ToolExecutionDisposition::Local,
            plakous_model::ToolCallerDisposition::DirectOnly,
        )]);
    }
    Ok(builder.build()?)
}

fn user(id: &str, text: &str) -> TestResult<ConversationItem> {
    Ok(ConversationItem::new(
        id.parse::<EntryId>()?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new(text, None)?)],
        Provenance::UserInput,
    )?)
}

fn output_replay(id: &str, text: &str) -> TestResult<ReplayEnvelope> {
    let native = serde_json::json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "status": "completed",
        "content": [{"type": "output_text", "text": text, "annotations": []}]
    });
    Ok(ReplayEnvelope::new(
        ProtocolId::new("openai.responses")?,
        None,
        ReplayScope::ExactAccountModel {
            model: plakous_model::ModelId::new("gpt-codex-artifact-test")?,
            account: account_binding(OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?,
        },
        ReplayKindId::new("responses.output_item")?,
        1,
        DataClassification::Sensitive,
        RawValue::from_string(native.to_string())?,
    )?)
}

fn assistant(id: &str, replay: ReplayEnvelope, text: &str) -> TestResult<ConversationItem> {
    Ok(ConversationItem::new(
        id.parse::<EntryId>()?,
        Actor::Assistant,
        vec![
            ContentBlock::Opaque(OpaqueBlock::new(replay, OpaqueFallback::CompatibleReplay)?),
            ContentBlock::Text(TextBlock::new(text, Some(TextPhase::Final))?),
        ],
        Provenance::ModelOutput,
    )?)
}

fn account_binding(route: &str) -> TestResult<Digest> {
    Ok(Digest::sha256(
        DigestDomain::ModelBinding,
        1,
        &[b"codex-artifact-test-account", route.as_bytes()],
    )?)
}

fn plan(request: &ModelRequest, route: &str) -> TestResult<ValidatedRequestPlan> {
    Ok(descriptor(route)?.validate_request(request.clone(), ValidationMode::Strict)?)
}

fn authority(request: &ModelRequest, route: &str) -> TestResult<ResponsesRequestAuthority> {
    let descriptor = descriptor(route)?;
    let plan = descriptor.validate_request(request.clone(), ValidationMode::Strict)?;
    Ok(ResponsesRequestAuthority::from_admitted_operation(
        &descriptor,
        &plan,
        request,
        account_binding(route)?,
        ResponsesSemanticRevisions::new(4, 1, 4)?,
        Some(Digest::sha256(
            DigestDomain::CacheAffinity,
            1,
            &[b"codex-artifact-test-affinity"],
        )?),
        ResponsesStoragePolicy::Stateless,
    )?)
}

fn artifact(
    producing: &ModelRequest,
    covered: &ModelRequest,
    replay: ReplayEnvelope,
    tag: &[u8],
) -> TestResult<ResponsesArtifact> {
    let descriptor = descriptor(OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?;
    let plan = descriptor.validate_request(producing.clone(), ValidationMode::Strict)?;
    let covered_projection = covered
        .input()
        .canonical_item_prefix_identity(producing.input().items().len().saturating_add(1))?;
    let production = ResponsesArtifactProduction::from_admitted_response(
        producing,
        &plan,
        Digest::sha256(
            DigestDomain::ProviderOperation,
            1,
            &[b"codex-artifact-test-physical", tag],
        )?,
        Digest::sha256(
            DigestDomain::ProviderOperation,
            1,
            &[b"codex-artifact-test-terminal", tag],
        )?,
        1,
        covered_projection.identity(),
    )?;
    let contract = ResponsesReplayContract::new(ResponsesArtifactReplayKind::new(
        ReplayKindId::new("responses.output_item")?,
        1,
    )?);
    Ok(ResponsesArtifact::new(
        ResponsesArtifactBinding::from_admitted_operation(
            &descriptor,
            &plan,
            producing,
            account_binding(OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?,
            ResponsesSemanticRevisions::new(4, 1, 4)?,
            Some(Digest::sha256(
                DigestDomain::CacheAffinity,
                1,
                &[b"codex-artifact-test-affinity"],
            )?),
        )?,
        production,
        contract,
        vec![ResponsesReplayItem::new(
            ResponsesReplayPurpose::OutputItem,
            0,
            replay,
        )],
        None,
        None,
        ResponsesStorageMode::Stateless,
        None,
        now()?,
    )?)
}

fn candidate(artifact: &ResponsesArtifact) -> TestResult<SealedFamilyArtifactCandidate> {
    let binding = artifact.binding();
    let production = artifact.production();
    let producer = FamilyArtifactProducerFacts::new(
        production.producer_request(),
        binding.model_input(),
        production.validated_plan(),
        binding.cache_affinity(),
        binding.run(),
        binding.root(),
        binding.attempt(),
        production.producing_projection(),
        production.producing_projection_items(),
        production.output_items(),
        production.covered_projection(),
        production.covered_projection_items(),
        production.physical_request(),
        production.terminal_response(),
    )?;
    Ok(SealedFamilyArtifactCandidate::new(
        "responses",
        RESPONSES_ARTIFACT_SCHEMA_REVISION,
        RESPONSES_ARTIFACT_CODEC_REVISION,
        binding.identity(),
        producer,
        ResponsesArtifactCodec::encode(artifact)?,
    )?)
}

fn active(candidate: &SealedFamilyArtifactCandidate) -> TestResult<ActiveFamilyArtifact> {
    Ok(ActiveFamilyArtifact::new(
        candidate.family(),
        candidate.schema_revision(),
        candidate.codec_revision(),
        candidate.binding(),
        candidate.bytes().to_vec(),
    )?)
}

fn transition_fixture() -> TestResult<TransitionFixture> {
    let first_user = user("00000000-0000-7000-8000-000000000310", "first")?;
    let first_segment = HistorySegment::new(HistorySegmentKind::RecentIntent, vec![first_user])?;
    let first = request(
        vec![first_segment.clone()],
        "00000000-0000-7000-8000-000000000311",
    )?;
    let replay = output_replay("codex-output-1", "answer one")?;
    let first_output = assistant(
        "00000000-0000-7000-8000-000000000312",
        replay.clone(),
        "answer one",
    )?;
    let output_segment = HistorySegment::new(
        HistorySegmentKind::ReasoningReplay,
        vec![first_output.clone()],
    )?;
    let second = request(
        vec![
            first_segment,
            output_segment,
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![user("00000000-0000-7000-8000-000000000313", "second")?],
            )?,
        ],
        "00000000-0000-7000-8000-000000000314",
    )?;
    let candidate = candidate(&artifact(&first, &second, replay, b"one")?)?;
    let active = active(&candidate)?;
    Ok(TransitionFixture {
        first,
        second,
        first_output,
        candidate,
        active,
    })
}

fn lower(
    request: &ModelRequest,
    bridge: Option<&plakous_responses_dialect::ResponsesArtifactReplayBridge>,
) -> TestResult<plakous_responses_dialect::LoweredResponsesRequest> {
    let descriptor = descriptor(OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?;
    Ok(lower_responses_request(
        request,
        ResponsesRequestConfig::new(
            descriptor.protocol(),
            descriptor.model_family(),
            descriptor.requested_model(),
            codex_request_policy(&descriptor, false),
            bridge.map(plakous_responses_dialect::ResponsesArtifactReplayBridge::context),
        ),
    )?)
}

fn native_request(source: &ModelRequest) -> TestResult<ModelRequest> {
    let mut native = BTreeMap::new();
    native.insert(
        plakous_model::NamespaceId::new("plakous.compaction")?,
        serde_json::json!("native_responses_trigger_v1"),
    );
    Ok(ModelRequest::builder(
        source.selected_provider_configuration_id(),
        source.effective_provider_configuration_id(),
        source.input().clone(),
        RequestMetadata::for_session_workflow(
            source.metadata().run_id(),
            plakous_model::SessionWorkflowRootId::new([0x35; 32])?,
            "00000000-0000-7000-8000-000000000351".parse()?,
            now()?,
        ),
    )
    .instructions(source.instructions().to_vec())
    .native(plakous_model::NativeOptions::new(native))
    .resolved_harness_plan_identity(harness_plan_identity()?)
    .build()?)
}

fn encrypted_transition_fixture() -> TestResult<TransitionFixture> {
    let mut fixture = transition_fixture()?;
    let replay = ReplayEnvelope::new(
        ProtocolId::new("openai.responses")?,
        None,
        ReplayScope::ExactAccountModel {
            model: plakous_model::ModelId::new("gpt-codex-artifact-test")?,
            account: account_binding(OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?,
        },
        ReplayKindId::new("responses.output_item")?,
        1,
        DataClassification::Sensitive,
        RawValue::from_string(
            serde_json::json!({
                "id":"encrypted-reasoning", "type":"reasoning", "summary":[],
                "encrypted_content":"A".repeat(228_168)
            })
            .to_string(),
        )?,
    )?;
    let reasoning = ConversationItem::new(
        "00000000-0000-7000-8000-000000000352".parse()?,
        Actor::Assistant,
        vec![ContentBlock::Reasoning(plakous_model::ReasoningBlock::new(
            plakous_model::ReasoningVisibility::OpaqueReplay,
            None,
            Some(replay),
        )?)],
        Provenance::ModelOutput,
    )?;
    let mut segments = fixture.first.input().history().segments().to_vec();
    segments.push(HistorySegment::new(
        HistorySegmentKind::ReasoningReplay,
        vec![reasoning],
    )?);
    fixture.first = request(segments.clone(), "00000000-0000-7000-8000-000000000353")?;
    let replay = output_replay("large-answer", &"x".repeat(62_000))?;
    fixture.first_output = assistant(
        "00000000-0000-7000-8000-000000000354",
        replay.clone(),
        &"x".repeat(62_000),
    )?;
    segments.push(HistorySegment::new(
        HistorySegmentKind::ReasoningReplay,
        vec![fixture.first_output.clone()],
    )?);
    fixture.second = request(segments, "00000000-0000-7000-8000-000000000355")?;
    fixture.candidate = candidate(&artifact(
        &fixture.first,
        &fixture.second,
        replay,
        b"encrypted",
    )?)?;
    fixture.active = active(&fixture.candidate)?;
    Ok(fixture)
}

#[test]
fn native_artifact_hard_refusals_keep_typed_preparation_provenance() -> TestResult {
    let fixture = transition_fixture()?;
    let request = native_request(&fixture.second)?;
    let mut bytes = fixture.active.bytes().to_vec();
    *bytes.last_mut().ok_or("artifact bytes")? ^= 1;
    let corrupt = ActiveFamilyArtifact::new(
        fixture.active.family(),
        fixture.active.schema_revision(),
        fixture.active.codec_revision(),
        fixture.active.binding(),
        bytes,
    )?;
    for (active, route) in [
        (&corrupt, OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID),
        (&fixture.active, "foreign.route"),
    ] {
        let error = codex_artifact_replay_bridge(
            Some(active),
            &request,
            authority(&request, route)?,
            now()?,
        )
        .expect_err("hard artifact refusal");
        let preparation = super::OpenAiLanguageModel::artifact_preparation_error(error);
        assert_eq!(
            preparation.rejection_kind(),
            Some(plakous_model::ModelPreparationErrorKind::ArtifactReuse)
        );
        assert_eq!(
            plakous_model::ModelError::from(preparation).kind(),
            plakous_model::ModelErrorKind::InvalidRequest
        );
    }
    Ok(())
}

#[test]
fn native_compaction_keeps_the_anchored_encrypted_replay_items_exact() -> TestResult {
    let fixture = encrypted_transition_fixture()?;
    let bridge = |request: &ModelRequest| -> TestResult<
        Option<plakous_responses_dialect::ResponsesArtifactReplayBridge>,
    > {
        codex_artifact_replay_bridge(
            Some(&fixture.active),
            request,
            authority(request, OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?,
            now()?,
        )
        .map_err(Into::into)
    };
    let ordinary = bridge(&fixture.second)?.ok_or("ordinary replay")?;
    let ordinary_wire = lower(&fixture.second, Some(&ordinary))?;
    let native = native_request(&fixture.second)?;
    let native_bridge = bridge(&native)?.ok_or("native replay must retain its source artifact")?;
    let native_wire = lower(&native, Some(&native_bridge))?;
    let mut expected = ordinary_wire.body()["input"]
        .as_array()
        .ok_or("input")?
        .clone();
    assert!(expected.iter().any(|item| item["type"] == "reasoning"));
    expected.push(serde_json::json!({"type":"compaction_trigger"}));
    assert_eq!(native_wire.body()["input"], serde_json::json!(expected));
    assert!(native_wire.body().get("previous_response_id").is_none());
    assert_ne!(
        lower(&native, None)?.body()["input"],
        native_wire.body()["input"]
    );

    // The closed refinement does not authorize changed canonical coverage.
    let changed = native_request(&fixture.first)?;
    assert!(bridge(&changed)?.is_none());
    let changed_prefix = ModelRequest::builder(
        fixture.second.selected_provider_configuration_id(),
        fixture.second.effective_provider_configuration_id(),
        fixture.second.input().clone(),
        fixture.second.metadata(),
    )
    .instructions(vec![plakous_model::InstructionBlock::new(
        plakous_model::InstructionSource::System,
        "changed instructions",
    )?])
    .resolved_harness_plan_identity(harness_plan_identity()?)
    .build()?;
    assert!(bridge(&native_request(&changed_prefix)?)?.is_none());
    assert!(matches!(
        codex_artifact_replay_bridge(
            Some(&fixture.active),
            &native,
            authority(&native, "foreign.route")?,
            now()?,
        ),
        Err(plakous_responses_dialect::ResponsesRequestError::Replay(
            plakous_responses_dialect::ResponsesReplayError::ArtifactTargetMismatch {
                axis: "route"
            }
        ))
    ));
    Ok(())
}

/// A compaction beside a live artifact prepares fresh instead of failing.
///
/// The Codex Responses route refused the whole preparation when the active
/// artifact covered a different canonical prefix, so a compaction fold over
/// a sealed cut could never be prepared while a turn's artifact was live.
/// The live Luna growth case lost five compactions to it.
#[test]
fn a_compaction_request_prepares_fresh_beside_a_live_artifact() -> TestResult {
    let fixture = transition_fixture()?;

    // The compaction's own request: a different canonical prefix from the
    // one the live artifact continues.
    let fold = request(
        vec![HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![user(
                "00000000-0000-7000-8000-000000000315",
                "fold the cut",
            )?],
        )?],
        "00000000-0000-7000-8000-000000000316",
    )?;
    let fold_authority = authority(&fold, OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?;
    let fold_bridge =
        codex_artifact_replay_bridge(Some(&fixture.active), &fold, fold_authority, now()?)?;
    assert!(
        fold_bridge.is_none(),
        "the compaction must prepare fresh, not refuse"
    );
    // And the wire it lowers to continues nothing: a sealed cut carries its
    // own coverage and asks the route to replay none of the live turn's.
    let fold_wire = lower(&fold, fold_bridge.as_ref())?;
    assert_eq!(fold_wire.replay_mode(), &ResponsesReplayMode::Full);
    assert!(
        fold_wire.body().get("previous_response_id").is_none(),
        "a compaction request must not continue a previous response"
    );
    assert_eq!(fold_wire.body()["input"].as_array().map(Vec::len), Some(1));

    // And the live turn still reuses its own artifact afterwards.
    let second_authority = authority(&fixture.second, OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?;
    assert!(
        codex_artifact_replay_bridge(
            Some(&fixture.active),
            &fixture.second,
            second_authority,
            now()?,
        )?
        .is_some(),
        "the live turn's own continuation still reuses its artifact"
    );
    Ok(())
}

#[test]
fn codex_stateless_first_second_third_and_reopen_context_are_exact() -> TestResult {
    let fixture = transition_fixture()?;
    let first_authority = authority(&fixture.first, OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?;
    let absent = codex_artifact_replay_bridge(None, &fixture.first, first_authority, now()?)?;
    assert!(absent.is_none());
    let first_wire = lower(&fixture.first, None)?;
    assert_eq!(first_wire.replay_mode(), &ResponsesReplayMode::Full);
    assert_eq!(first_wire.body()["input"].as_array().map(Vec::len), Some(1));
    assert!(first_wire.body().get("previous_response_id").is_none());

    let second_authority = authority(&fixture.second, OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?;
    assert_ne!(
        fixture.active.binding(),
        second_authority.artifact_binding().identity(),
        "the wrapper must preserve its producer binding"
    );
    let second_bridge = codex_artifact_replay_bridge(
        Some(&fixture.active),
        &fixture.second,
        second_authority,
        now()?,
    )?
    .ok_or("active first artifact was not bridged")?;
    let second_wire = lower(&fixture.second, Some(&second_bridge))?;
    assert_eq!(second_wire.replay_mode(), &ResponsesReplayMode::Full);
    assert!(
        second_wire.body()["input"]
            .as_array()
            .is_some_and(|input| input.iter().any(|item| item["id"] == "codex-output-1"))
    );
    assert!(second_wire.body().get("previous_response_id").is_none());

    let second_replay = output_replay("codex-output-2", "answer two")?;
    let second_output = assistant(
        "00000000-0000-7000-8000-000000000315",
        second_replay.clone(),
        "answer two",
    )?;
    let mut third_segments = fixture.second.input().history().segments().to_vec();
    third_segments.push(HistorySegment::new(
        HistorySegmentKind::ReasoningReplay,
        vec![second_output],
    )?);
    third_segments.push(HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![user("00000000-0000-7000-8000-000000000316", "third")?],
    )?);
    let third = request(third_segments, "00000000-0000-7000-8000-000000000317")?;
    let second_candidate = candidate(&artifact(&fixture.second, &third, second_replay, b"two")?)?;
    let reopened: ActiveFamilyArtifact =
        serde_json::from_value(serde_json::to_value(active(&second_candidate)?)?)?;
    let third_bridge = codex_artifact_replay_bridge(
        Some(&reopened),
        &third,
        authority(&third, OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?,
        now()?,
    )?
    .ok_or("reopened second artifact was not bridged")?;
    let third_wire = lower(&third, Some(&third_bridge))?;
    let third_input = third_wire.body()["input"]
        .as_array()
        .ok_or("third request input was not an array")?;
    assert!(
        third_input
            .iter()
            .any(|item| item["id"] == "codex-output-1")
    );
    assert!(
        third_input
            .iter()
            .any(|item| item["id"] == "codex-output-2")
    );
    assert!(third_wire.body().get("previous_response_id").is_none());
    Ok(())
}

#[test]
fn codex_complete_input_bound_covers_minimal_unicode_tools_and_replay() -> TestResult {
    let minimal = text_request("x", "00000000-0000-7000-8000-000000000390", false)?;
    let unicode = text_request(
        "é中🦀\\\"\\n",
        "00000000-0000-7000-8000-000000000391",
        false,
    )?;
    let tool = text_request("tool input", "00000000-0000-7000-8000-000000000392", true)?;
    let fixture = transition_fixture()?;
    let replay_bridge = codex_artifact_replay_bridge(
        Some(&fixture.active),
        &fixture.second,
        authority(&fixture.second, OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?,
        now()?,
    )?
    .ok_or("replay fixture did not produce a bridge")?;
    let bodies = [
        serde_json::to_vec(lower(&minimal, None)?.body())?,
        serde_json::to_vec(lower(&unicode, None)?.body())?,
        serde_json::to_vec(lower(&tool, None)?.body())?,
        serde_json::to_vec(lower(&fixture.second, Some(&replay_bridge))?.body())?,
    ];
    for body in bodies {
        let wire_size = u64::try_from(body.len())?;
        let bound = codex_complete_input_token_upper_bound(wire_size)
            .ok_or("complete-input bound overflowed")?;
        let expected = wire_size.saturating_add(CODEX_COMPLETE_INPUT_BYTES_PER_TOKEN - 1)
            / CODEX_COMPLETE_INPUT_BYTES_PER_TOKEN;
        assert_eq!(bound, expected);
        assert_eq!(
            codex_reference_client_input_tokens(&body).ok_or("reference-client bound")?,
            bound,
            "bodies without compaction items keep the ordinary ceil(bytes/4) envelope"
        );
        assert!(bound <= wire_size, "bytes/4 must not exceed wire bytes");
        assert!(bound >= wire_size / CODEX_COMPLETE_INPUT_BYTES_PER_TOKEN);
    }
    Ok(())
}

#[test]
fn codex_complete_input_bound_exact_fit_one_overflow_and_arithmetic_failure() -> TestResult {
    let request = text_request("boundary", "00000000-0000-7000-8000-000000000393", true)?;
    let body = serde_json::to_vec(lower(&request, None)?.body())?;
    let measured = codex_complete_input_token_upper_bound(u64::try_from(body.len())?)
        .ok_or("complete-input bound overflowed")?;
    let exact_envelope = measured;
    assert!(measured <= exact_envelope, "exact fit must be admitted");
    assert_eq!(
        measured.checked_sub(exact_envelope - 1),
        Some(1),
        "a one-token-smaller envelope must overflow by exactly one"
    );
    assert!(codex_complete_input_token_upper_bound(u64::MAX).is_none());
    Ok(())
}

#[test]
fn codex_complete_input_bound_is_ceil_bytes_div_4() {
    assert_eq!(codex_complete_input_token_upper_bound(0), Some(0));
    assert_eq!(codex_complete_input_token_upper_bound(1), Some(1));
    assert_eq!(codex_complete_input_token_upper_bound(4), Some(1));
    assert_eq!(codex_complete_input_token_upper_bound(5), Some(2));
    assert_eq!(codex_complete_input_token_upper_bound(40_000), Some(10_000));
}

#[test]
fn codex_reference_client_discounts_encrypted_reasoning_and_compaction() -> TestResult {
    for kind in ["reasoning", "compaction"] {
        for length in [0, 867, 868, 100_000] {
            let ciphertext = "A".repeat(length);
            let body = serde_json::to_vec(&serde_json::json!({
                "model": "gpt-5.4",
                "input": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "continue"}]
                    },
                    {
                        "type": kind,
                        "encrypted_content": ciphertext
                    }
                ],
                "stream": true
            }))?;
            let naive = codex_complete_input_token_upper_bound(u64::try_from(body.len())?)
                .ok_or("naive bound")?;
            let measured =
                codex_reference_client_input_tokens(&body).ok_or("reference-client bound")?;
            assert!(
                measured < naive,
                "{kind} ciphertext must not be charged as ordinary JSON utf8/4 ({measured} < {naive})"
            );
            let visible = (u64::try_from(length)? * 3 / 4).saturating_sub(650);
            let item = serde_json::json!({
                "type": kind,
                "encrypted_content": ciphertext
            });
            let item_len = u64::try_from(serde_json::to_vec(&item)?.len())?;
            let adjusted = u64::try_from(body.len())?
                .checked_sub(item_len)
                .ok_or("item larger than body")?
                .checked_add(visible)
                .ok_or("visible overflow")?;
            assert_eq!(
                measured,
                codex_complete_input_token_upper_bound(adjusted).ok_or("expected bound")?
            );
        }
    }
    Ok(())
}

#[test]
fn codex_reference_meter_does_not_discount_unrecognized_or_missing_ciphertext() -> TestResult {
    for item in [
        serde_json::json!({"type":"reasoning","summary":[{"type":"summary_text","text":"visible reasoning"}]}),
        serde_json::json!({"type":"reasoning","encrypted_content":null}),
        serde_json::json!({"type":"reasoning","encrypted_content":123}),
        serde_json::json!({"type":"message","encrypted_content":"A".repeat(10_000)}),
    ] {
        let body = serde_json::to_vec(&serde_json::json!({"input":[item]}))?;
        assert_eq!(
            codex_reference_client_input_tokens(&body),
            codex_complete_input_token_upper_bound(u64::try_from(body.len())?)
        );
    }
    Ok(())
}

#[test]
fn codex_active_artifact_tamper_binding_revision_and_family_fail_closed() -> TestResult {
    let fixture = transition_fixture()?;
    let bytes = fixture.candidate.bytes().to_vec();
    let binding = fixture.candidate.binding();
    let mut tampered = bytes.clone();
    *tampered.last_mut().ok_or("empty artifact bytes")? ^= 1;
    let invalid = [
        ActiveFamilyArtifact::new(
            "messages",
            fixture.candidate.schema_revision(),
            fixture.candidate.codec_revision(),
            binding,
            bytes.clone(),
        )?,
        ActiveFamilyArtifact::new(
            "responses",
            fixture.candidate.schema_revision().saturating_add(1),
            fixture.candidate.codec_revision(),
            binding,
            bytes.clone(),
        )?,
        ActiveFamilyArtifact::new(
            "responses",
            fixture.candidate.schema_revision(),
            fixture.candidate.codec_revision().saturating_add(1),
            binding,
            bytes.clone(),
        )?,
        ActiveFamilyArtifact::new(
            "responses",
            fixture.candidate.schema_revision(),
            fixture.candidate.codec_revision(),
            Digest::sha256(
                DigestDomain::ModelBinding,
                1,
                &[b"foreign-neutral-wrapper-binding"],
            )?,
            bytes,
        )?,
        ActiveFamilyArtifact::new(
            "responses",
            fixture.candidate.schema_revision(),
            fixture.candidate.codec_revision(),
            binding,
            tampered,
        )?,
    ];
    for artifact in &invalid {
        assert!(
            codex_artifact_replay_bridge(
                Some(artifact),
                &fixture.second,
                authority(&fixture.second, OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?,
                now()?,
            )
            .is_err()
        );
    }
    assert!(
        codex_artifact_replay_bridge(
            Some(&fixture.active),
            &fixture.second,
            authority(&fixture.second, "codex-foreign-route")?,
            now()?,
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn codex_terminal_uses_sidecar_and_only_item_level_replay() -> TestResult {
    let fixture = transition_fixture()?;
    let first_plan = plan(&fixture.first, OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?;
    let covered = first_plan
        .request()
        .input()
        .staged_canonical_item_prefix_identity(std::slice::from_ref(&fixture.first_output))?;
    fixture
        .candidate
        .validate_for_commit(&first_plan, covered, 1)?;
    let response = ModelResponse::new(
        vec![fixture.first_output],
        FinishDetails::new(FinishReason::Stop, None),
        None,
        Some("codex-response-sidecar".to_owned()),
        first_plan.digest(),
    )
    .with_family_artifact_candidate(fixture.candidate)?;
    assert!(response.family_artifact_candidate().is_some());
    assert!(
        !serde_json::to_value(&response)?
            .as_object()
            .is_some_and(|object| object.contains_key("replay"))
    );
    let replay_kinds = response
        .output()
        .iter()
        .flat_map(ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::Opaque(opaque) => Some(opaque.replay().kind().as_str()),
            ContentBlock::Reasoning(reasoning) => {
                reasoning.replay().map(|replay| replay.kind().as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(replay_kinds, vec!["responses.output_item"]);
    let encoded = serde_json::to_string(&response)?;
    assert!(!encoded.contains("responses.artifact"));
    assert!(!encoded.contains("\"encoding\":\"base64\""));
    Ok(())
}

#[test]
fn codex_tool_output_coverage_is_open_until_the_result_arrives() -> TestResult {
    let intent = HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![user(
            "00000000-0000-7000-8000-000000000320",
            "use the tool",
        )?],
    )?;
    let first = request(vec![intent.clone()], "00000000-0000-7000-8000-000000000321")?;
    let first_plan = plan(&first, OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?;
    let correlation = ToolCorrelation::new(
        Some("codex-call-1".to_owned()),
        None,
        plakous_model::ToolCallerLineage::Direct,
    )?;
    let call_id = "00000000-0000-7000-8000-000000000322".parse::<CallId>()?;
    let call = ToolCallBlock::new(
        call_id,
        ToolName::new("lookup")?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        correlation.clone(),
        r#"{"q":"fixture"}"#,
    )?;
    let call_item = ConversationItem::new(
        "00000000-0000-7000-8000-000000000323".parse::<EntryId>()?,
        Actor::Assistant,
        vec![ContentBlock::ToolCall(call)],
        Provenance::ModelOutput,
    )?;
    let covered = first_plan
        .request()
        .input()
        .staged_canonical_item_prefix_identity(std::slice::from_ref(&call_item))?;
    let result = ToolResultBlock::new(
        call_id,
        ToolExecutionOwner::Local,
        ToolResultKind::Success,
        vec![ToolResultPart::Text(TextBlock::new(
            "fixture result",
            None,
        )?)],
        correlation,
    )?;
    let result_item = ConversationItem::new(
        "00000000-0000-7000-8000-000000000324".parse::<EntryId>()?,
        Actor::Tool,
        vec![ContentBlock::ToolResult(result)],
        Provenance::ToolExecution,
    )?;
    let exchange = HistorySegment::new(
        HistorySegmentKind::ToolExchange,
        vec![call_item, result_item],
    )?;
    let next = ConversationView::from_shared(
        CanonicalHistory::new(vec![intent, exchange])?,
        WorkingProjection::default(),
    );
    assert_eq!(covered, next.canonical_item_prefix_identity(2)?);
    Ok(())
}
