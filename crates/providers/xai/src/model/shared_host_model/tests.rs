#![allow(
    clippy::too_many_lines,
    reason = "Exercise exact SuperGrok preparation and artifact tamper, binding, revision and authority refusals end to end."
)]
use std::collections::BTreeMap;
use std::error::Error;
use std::str::FromStr;

use plakous_model::{
    ActiveFamilyArtifact, Actor, AttemptId, CanonicalHistory, Capability, CapabilityConstraints,
    CapabilitySet, ContentBlock, ConversationView, DataClassification, Digest, DigestDomain,
    EffectiveProviderConfigurationId, EntryId, FactProvenance, FactSource,
    FamilyArtifactProducerFacts, FeatureSupport, FinishDetails, FinishReason, HistorySegment,
    HistorySegmentKind, ModelAdapterProfileId, ModelFamilyId, ModelId, ModelRequest, ModelResponse,
    OpaqueBlock, OpaqueFallback, ProtocolId, Provenance, ReplayEnvelope, ReplayKindId, ReplayScope,
    RequestMetadata, RouteId, RunId, SealedFamilyArtifactCandidate,
    SelectedProviderConfigurationId, Support, TextBlock, TextPhase, ToolCallBlock, ToolCallState,
    ToolCorrelation, ToolExecutionOwner, ToolName, ToolResultBlock, ToolResultKind, ToolResultPart,
    TurnId, ValidationMode, WorkingProjection,
};
use plakous_responses::{
    RESPONSES_ARTIFACT_CODEC_REVISION, RESPONSES_ARTIFACT_SCHEMA_REVISION, ResponsesArtifact,
    ResponsesArtifactBinding, ResponsesArtifactCodec, ResponsesArtifactProduction,
    ResponsesArtifactReplayKind, ResponsesReplayContract, ResponsesReplayItem,
    ResponsesReplayPurpose,
};
use plakous_responses_dialect::{
    ResponsesAttemptBuildError, ResponsesRequestConfig, lower_responses_request,
};
use serde_json::json;
use serde_json::value::RawValue;

use super::*;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const MODEL: &str = "grok-build-0.1";

#[test]
fn native_operation_budget_still_obeys_earlier_caller_deadline() -> TestResult {
    let clock = plakous_model::FixedClock::new(Timestamp::new(1_000, 0)?);
    let before = Instant::now();
    let full = operation_deadline(&clock, Duration::from_secs(300), None);
    let earlier = operation_deadline(
        &clock,
        Duration::from_secs(300),
        Some(Timestamp::new(1_180, 0)?),
    );
    assert!(full.duration_since(before) >= Duration::from_secs(299));
    assert!(earlier.duration_since(before) >= Duration::from_secs(179));
    assert!(earlier.duration_since(before) < Duration::from_secs(181));
    assert!(earlier < full);
    Ok(())
}

fn now() -> Timestamp {
    Timestamp::new(1_800_000_000, 0).expect("fixed time")
}

fn test_descriptor(model: &str, route: &str) -> TestResult<ResolvedModelDescriptor> {
    let mut features = BTreeMap::new();
    features.insert(
        Capability::Streaming,
        FeatureSupport::new(
            Support::Native,
            CapabilityConstraints::default(),
            FactProvenance::new(FactSource::TestFixture, now(), None),
        ),
    );
    Ok(ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new(SUPERGROK_RESPONSES_DIALECT.protocol_id())?,
        RouteId::new(route)?,
        ModelId::new(model)?,
        Some(ModelId::new(model)?),
        Some(ModelFamilyId::new("grok")?),
        ModelAdapterProfileId::new("xai.grok.compat.2026-07-31.r1")?,
        CapabilitySet::new(features, BTreeMap::new()),
    ))
}

fn harness_plan() -> TestResult<Digest> {
    Ok(Digest::sha256(
        DigestDomain::ResolvedHarnessPlan,
        1,
        &[b"xai-model-artifact-test-plan"],
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
            &[b"xai-model-test-selected-configuration"],
        )?)?)?,
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"xai-model-test-effective-configuration"],
        )?)?)?,
    ))
}

fn metadata(round: u32) -> TestResult<RequestMetadata> {
    let suffix = |offset: u32| format!("00000000-0000-4000-8000-{:012}", round * 10 + offset);
    Ok(RequestMetadata::for_turn(
        RunId::from_str("00000000-0000-4000-8000-000000000001")?,
        TurnId::from_str(&suffix(1))?,
        AttemptId::from_str(&suffix(2))?,
        now(),
    ))
}

fn user_segment(round: u32) -> TestResult<HistorySegment> {
    let id = format!("00000000-0000-4000-8000-{:012}", 100 + round);
    let item = plakous_model::ConversationItem::new(
        EntryId::from_str(&id)?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new(
            format!("intent {round}"),
            None,
        )?)],
        Provenance::UserInput,
    )?;
    Ok(HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![item],
    )?)
}

fn request(segments: Vec<HistorySegment>, round: u32) -> TestResult<ModelRequest> {
    let (selected, effective) = provider_configuration_ids()?;
    Ok(ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_shared(
            CanonicalHistory::new(segments)?,
            WorkingProjection::default(),
        ),
        metadata(round)?,
    )
    .resolved_harness_plan_identity(harness_plan()?)
    .build()?)
}

fn tool_free_request(segments: Vec<HistorySegment>, round: u32) -> TestResult<ModelRequest> {
    let (selected, effective) = provider_configuration_ids()?;
    Ok(ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_shared(
            CanonicalHistory::new(segments)?,
            WorkingProjection::default(),
        ),
        metadata(round)?,
    )
    .resolved_harness_plan_identity(harness_plan()?)
    .tool_choice(plakous_model::ToolChoice::None)
    .build()?)
}

fn account() -> TestResult<Digest> {
    Ok(Digest::sha256(
        DigestDomain::ModelBinding,
        1,
        &[b"xai-model-artifact-test-account"],
    )?)
}

fn authority(
    descriptor: &ResolvedModelDescriptor,
    plan: &ValidatedRequestPlan,
    revisions: ResponsesSemanticRevisions,
) -> TestResult<ResponsesRequestAuthority> {
    Ok(ResponsesRequestAuthority::from_admitted_operation(
        descriptor,
        plan,
        plan.request(),
        account()?,
        revisions,
        Some(Digest::sha256(
            DigestDomain::CacheAffinity,
            1,
            &[b"xai-model-artifact-test-affinity"],
        )?),
        ResponsesStoragePolicy::Stateless,
    )?)
}

fn replay_envelope(
    descriptor: &ResolvedModelDescriptor,
    raw: serde_json::Value,
    kind: plakous_responses_dialect::ResponsesReplayKind<'_>,
) -> TestResult<ReplayEnvelope> {
    Ok(ReplayEnvelope::new(
        descriptor.protocol().clone(),
        descriptor.model_family().cloned(),
        ReplayScope::ExactAccountModel {
            model: descriptor.requested_model().clone(),
            account: account()?,
        },
        ReplayKindId::new(kind.name())?,
        kind.version(),
        DataClassification::Sensitive,
        RawValue::from_string(raw.to_string())?,
    )?)
}

fn canonical_output(
    descriptor: &ResolvedModelDescriptor,
    round: u32,
) -> TestResult<(
    Vec<plakous_model::ConversationItem>,
    Vec<ResponsesReplayItem>,
)> {
    let output_kind = SUPERGROK_RESPONSES_DIALECT.output_item_replay();
    let opaque_kind = SUPERGROK_RESPONSES_DIALECT.opaque_output_replay();
    let reasoning = replay_envelope(
        descriptor,
        json!({
            "id":format!("reasoning_{round}"),
            "type":"reasoning",
            "status":"completed",
            "summary":[],
            "encrypted_content":format!("encrypted-{round}")
        }),
        output_kind,
    )?;
    let message = replay_envelope(
        descriptor,
        json!({
            "id":format!("message_{round}"),
            "type":"message",
            "status":"completed",
            "role":"assistant",
            "content":[{
                "type":"output_text",
                "text":format!("answer {round}"),
                "annotations":[]
            }]
        }),
        output_kind,
    )?;
    let future = replay_envelope(
        descriptor,
        json!({
            "id":format!("future_{round}"),
            "type":"future_output",
            "status":"completed",
            "payload":{"opaque":format!("opaque-{round}")}
        }),
        opaque_kind,
    )?;
    let entry = |offset: u32| {
        EntryId::from_str(&format!(
            "00000000-0000-4000-8000-{:012}",
            1_000 + round * 10 + offset
        ))
    };
    let output = vec![
        plakous_model::ConversationItem::new(
            entry(1)?,
            Actor::Assistant,
            vec![ContentBlock::Opaque(OpaqueBlock::new(
                reasoning.clone(),
                OpaqueFallback::CompatibleReplay,
            )?)],
            Provenance::ModelOutput,
        )?,
        plakous_model::ConversationItem::new(
            entry(2)?,
            Actor::Assistant,
            vec![
                ContentBlock::Text(TextBlock::new(
                    format!("answer {round}"),
                    Some(TextPhase::Final),
                )?),
                ContentBlock::Opaque(OpaqueBlock::new(
                    message.clone(),
                    OpaqueFallback::CompatibleReplay,
                )?),
            ],
            Provenance::ModelOutput,
        )?,
        plakous_model::ConversationItem::new(
            entry(3)?,
            Actor::Assistant,
            vec![ContentBlock::Opaque(OpaqueBlock::new(
                future.clone(),
                OpaqueFallback::CompatibleReplay,
            )?)],
            Provenance::ModelOutput,
        )?,
    ];
    let replay = vec![
        ResponsesReplayItem::new(ResponsesReplayPurpose::OutputItem, 0, reasoning),
        ResponsesReplayItem::new(ResponsesReplayPurpose::OutputItem, 1, message),
        ResponsesReplayItem::new(ResponsesReplayPurpose::OpaqueOutputItem, 2, future),
    ];
    Ok((output, replay))
}

/// One historical assistant refusal and its exact native replay.
fn refusal_output(
    descriptor: &ResolvedModelDescriptor,
    round: u32,
    output_index: u32,
) -> TestResult<(plakous_model::ConversationItem, ResponsesReplayItem)> {
    let refusal = replay_envelope(
        descriptor,
        json!({
            "id":format!("refusal_{round}"),
            "type":"message",
            "status":"completed",
            "role":"assistant",
            "content":[{"type":"refusal","refusal":format!("refused {round}")}]
        }),
        SUPERGROK_RESPONSES_DIALECT.output_item_replay(),
    )?;
    let item = plakous_model::ConversationItem::new(
        EntryId::from_str(&format!(
            "00000000-0000-4000-8000-{:012}",
            1_000 + round * 10 + 9
        ))?,
        Actor::Assistant,
        vec![
            ContentBlock::Refusal(plakous_model::RefusalBlock::new(format!(
                "refused {round}"
            ))?),
            ContentBlock::Opaque(OpaqueBlock::new(
                refusal.clone(),
                OpaqueFallback::CompatibleReplay,
            )?),
        ],
        Provenance::ModelOutput,
    )?;
    Ok((
        item,
        ResponsesReplayItem::new(ResponsesReplayPurpose::OutputItem, output_index, refusal),
    ))
}

fn seal_round(
    descriptor: &ResolvedModelDescriptor,
    plan: &ValidatedRequestPlan,
    round: u32,
) -> TestResult<(ModelResponse, ResponsesArtifactBinding)> {
    let (output, replay) = canonical_output(descriptor, round)?;
    seal_output(descriptor, plan, round, output, replay)
}

fn seal_output(
    descriptor: &ResolvedModelDescriptor,
    plan: &ValidatedRequestPlan,
    round: u32,
    output: Vec<plakous_model::ConversationItem>,
    replay: Vec<ResponsesReplayItem>,
) -> TestResult<(ModelResponse, ResponsesArtifactBinding)> {
    let covered = covered_projection_after_output(plan, &output)?;
    let binding = authority(descriptor, plan, supergrok_semantic_revisions()?)?
        .artifact_binding()
        .clone();
    let round_bytes = round.to_be_bytes();
    let physical = Digest::sha256(
        DigestDomain::ProviderOperation,
        1,
        &[b"xai-model-artifact-physical", &round_bytes],
    )?;
    let terminal = Digest::sha256(
        DigestDomain::ProviderOperation,
        1,
        &[b"xai-model-artifact-terminal", &round_bytes],
    )?;
    let production = ResponsesArtifactProduction::from_admitted_response(
        plan.request(),
        plan,
        physical,
        terminal,
        u32::try_from(output.len())?,
        covered.identity(),
    )?;
    let contract = ResponsesReplayContract::new(ResponsesArtifactReplayKind::new(
        ReplayKindId::new(SUPERGROK_RESPONSES_DIALECT.output_item_replay().name())?,
        SUPERGROK_RESPONSES_DIALECT.output_item_replay().version(),
    )?)
    .with_opaque_output(ResponsesArtifactReplayKind::new(
        ReplayKindId::new(SUPERGROK_RESPONSES_DIALECT.opaque_output_replay().name())?,
        SUPERGROK_RESPONSES_DIALECT.opaque_output_replay().version(),
    )?)?;
    let artifact = ResponsesArtifact::new(
        binding.clone(),
        production.clone(),
        contract,
        replay,
        None,
        None,
        ResponsesStorageMode::Stateless,
        None,
        now(),
    )?;
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
    let candidate = SealedFamilyArtifactCandidate::new(
        "responses",
        RESPONSES_ARTIFACT_SCHEMA_REVISION,
        RESPONSES_ARTIFACT_CODEC_REVISION,
        binding.identity(),
        producer,
        ResponsesArtifactCodec::encode(&artifact)?,
    )?;
    candidate.validate_for_commit(plan, covered, output.len())?;
    let response = ModelResponse::new(
        output,
        FinishDetails::new(FinishReason::Stop, None),
        None,
        Some(format!("resp_{round}")),
        plan.digest(),
    )
    .with_family_artifact_candidate(candidate)?;
    Ok((response, binding))
}

fn active(response: &ModelResponse) -> TestResult<ActiveFamilyArtifact> {
    let candidate = response
        .family_artifact_candidate()
        .ok_or_else(|| std::io::Error::other("missing family artifact candidate"))?;
    Ok(ActiveFamilyArtifact::new(
        candidate.family(),
        candidate.schema_revision(),
        candidate.codec_revision(),
        candidate.binding(),
        candidate.bytes().to_vec(),
    )?)
}

fn output_segment(response: &ModelResponse) -> TestResult<HistorySegment> {
    Ok(HistorySegment::new(
        HistorySegmentKind::ReasoningReplay,
        response.output().to_vec(),
    )?)
}

fn assert_item_replay_without_aggregate(response: &ModelResponse) -> TestResult {
    let candidate = response
        .family_artifact_candidate()
        .ok_or_else(|| std::io::Error::other("missing family artifact candidate"))?;
    assert_eq!(candidate.family(), "responses");
    assert_eq!(
        candidate.schema_revision(),
        RESPONSES_ARTIFACT_SCHEMA_REVISION
    );
    assert_eq!(
        candidate.codec_revision(),
        RESPONSES_ARTIFACT_CODEC_REVISION
    );
    assert!(candidate.bytes().starts_with(b"PLAKRSP\0"));
    assert!(
        !serde_json::to_value(response)?
            .as_object()
            .is_some_and(|object| object.contains_key("replay")),
        "aggregate replay field is forbidden"
    );
    let kinds = response
        .output()
        .iter()
        .flat_map(plakous_model::ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::Opaque(opaque) => Some(opaque.replay().kind().as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(kinds.contains(&SUPERGROK_RESPONSES_DIALECT.output_item_replay().name()));
    assert!(kinds.contains(&SUPERGROK_RESPONSES_DIALECT.opaque_output_replay().name()));
    let canonical = serde_json::to_vec(response.output())?;
    assert!(!canonical.windows(7).any(|window| window == b"PLAKRSP"));
    assert!(
        !canonical
            .windows(b"responses.artifact".len())
            .any(|window| window == b"responses.artifact")
    );
    Ok(())
}

#[test]
fn stateless_artifact_chains_one_two_three_and_reopens_context() -> TestResult {
    let descriptor = test_descriptor(MODEL, "xai-test-route")?;
    let revisions = supergrok_semantic_revisions()?;
    let mut segments = vec![user_segment(1)?];
    let first_request = request(segments.clone(), 1)?;
    let first_plan = descriptor.validate_request(first_request, ValidationMode::Strict)?;
    let (first, first_binding) = seal_round(&descriptor, &first_plan, 1)?;
    assert_item_replay_without_aggregate(&first)?;
    segments.push(output_segment(&first)?);
    segments.push(user_segment(2)?);

    let second_request = request(segments.clone(), 2)?;
    let second_plan = descriptor.validate_request(second_request, ValidationMode::Strict)?;
    let second_authority = authority(&descriptor, &second_plan, revisions)?;
    assert_ne!(
        first_binding.identity(),
        second_authority.artifact_binding().identity()
    );
    let first_active = active(&first)?;
    let second_bridge = active_replay_bridge(
        Some(&first_active),
        second_plan.request(),
        second_authority,
        now(),
    )?
    .ok_or_else(|| std::io::Error::other("second round omitted replay bridge"))?;
    let second_wire = lower_responses_request(
        second_plan.request(),
        ResponsesRequestConfig::new(
            descriptor.protocol(),
            descriptor.model_family(),
            descriptor.requested_model(),
            SUPERGROK_RESPONSES_DIALECT.request_policy(),
            Some(second_bridge.context()),
        ),
    )?;
    let second_wire = second_wire.body().to_string();
    assert!(second_wire.contains("reasoning_1"));
    assert!(second_wire.contains("encrypted-1"));
    assert!(second_wire.contains("future_1"));
    assert!(!second_wire.contains("PLAKRSP"));
    assert!(!second_wire.contains("responses.artifact"));

    let (second, second_binding) = seal_round(&descriptor, &second_plan, 2)?;
    assert_item_replay_without_aggregate(&second)?;
    let second_candidate = second
        .family_artifact_candidate()
        .ok_or_else(|| std::io::Error::other("missing second candidate"))?;
    assert_eq!(second_candidate.binding(), second_binding.identity());
    segments.push(output_segment(&second)?);
    segments.push(user_segment(3)?);

    let third_request = request(segments, 3)?;
    let third_plan = descriptor.validate_request(third_request, ValidationMode::Strict)?;
    let third_authority = authority(&descriptor, &third_plan, revisions)?;
    assert_ne!(
        second_binding.identity(),
        third_authority.artifact_binding().identity()
    );
    let serialized = serde_json::to_vec(&active(&second)?)?;
    let reopened: ActiveFamilyArtifact = serde_json::from_slice(&serialized)?;
    let third_bridge = active_replay_bridge(
        Some(&reopened),
        third_plan.request(),
        third_authority,
        now(),
    )?
    .ok_or_else(|| std::io::Error::other("reopened third round omitted replay bridge"))?;
    let third_wire = lower_responses_request(
        third_plan.request(),
        ResponsesRequestConfig::new(
            descriptor.protocol(),
            descriptor.model_family(),
            descriptor.requested_model(),
            SUPERGROK_RESPONSES_DIALECT.request_policy(),
            Some(third_bridge.context()),
        ),
    )?
    .body()
    .to_string();
    assert!(third_wire.contains("reasoning_1"));
    assert!(third_wire.contains("reasoning_2"));
    assert!(third_wire.contains("encrypted-2"));
    assert!(third_wire.contains("future_2"));
    assert!(!third_wire.contains("PLAKRSP"));

    let (third, third_binding) = seal_round(&descriptor, &third_plan, 3)?;
    assert_item_replay_without_aggregate(&third)?;
    assert_eq!(
        third
            .family_artifact_candidate()
            .ok_or_else(|| std::io::Error::other("missing third candidate"))?
            .binding(),
        third_binding.identity()
    );
    Ok(())
}

/// A compaction beside a live artifact prepares fresh instead of failing.
///
/// An active artifact continues one exact canonical prefix. A
/// compaction fold covers a sealed cut, which is a different prefix, so
/// the artifact is simply not this request's. Refusing the whole
/// preparation is what killed every compaction the live Grok and Codex
/// growth turns attempted: "Responses artifact canonical coverage
/// differs from the current request", with the fold rung dead for the
/// rest of the session.
#[test]
fn a_compaction_request_prepares_fresh_beside_a_live_artifact() -> TestResult {
    let descriptor = test_descriptor(MODEL, "xai-test-route")?;
    let first_request = request(vec![user_segment(1)?], 1)?;
    let first_plan = descriptor.validate_request(first_request, ValidationMode::Strict)?;
    let (first, _) = seal_round(&descriptor, &first_plan, 1)?;
    let first_active = active(&first)?;

    // The compaction's own request: a different canonical prefix.
    let fold_request = request(vec![user_segment(2)?], 2)?;
    let fold_plan = descriptor.validate_request(fold_request, ValidationMode::Strict)?;
    let fold_authority = authority(&descriptor, &fold_plan, supergrok_semantic_revisions()?)?;
    let fold_bridge = active_replay_bridge(
        Some(&first_active),
        fold_plan.request(),
        fold_authority,
        now(),
    )?;
    assert!(
        fold_bridge.is_none(),
        "the compaction must prepare fresh, not refuse"
    );
    // And the wire it lowers to continues nothing: a sealed cut carries
    // its own coverage and asks the route to replay none of the live
    // turn's.
    let fold_wire = lower_responses_request(
        fold_plan.request(),
        ResponsesRequestConfig::new(
            descriptor.protocol(),
            descriptor.model_family(),
            descriptor.requested_model(),
            SUPERGROK_RESPONSES_DIALECT.request_policy(),
            fold_bridge
                .as_ref()
                .map(ResponsesArtifactReplayBridge::context),
        ),
    )?;
    assert!(
        fold_wire
            .body()
            .to_string()
            .find("previous_response_id")
            .is_none(),
        "a compaction request must not continue a previous response"
    );

    // And the main turn still reuses its own artifact afterwards.
    let second_request = request(
        vec![user_segment(1)?, output_segment(&first)?, user_segment(2)?],
        2,
    )?;
    let second_plan = descriptor.validate_request(second_request, ValidationMode::Strict)?;
    let second_authority = authority(&descriptor, &second_plan, supergrok_semantic_revisions()?)?;
    assert!(
        active_replay_bridge(
            Some(&first_active),
            second_plan.request(),
            second_authority,
            now(),
        )?
        .is_some(),
        "the live turn's own continuation still reuses its artifact"
    );
    Ok(())
}

/// The operation-budget wrap-up round keeps the live artifact's replay.
///
/// The wrap-up keeps the live history and prefix but withdraws tool
/// selection, which the artifact binding leaves out of its prefix
/// identity. Its history holds a retained unknown output item and an
/// assistant refusal, which fresh lowering refuses as
/// `ProtectedHistory`: preparing it fresh once failed the whole turn.
/// The next turn's first round, back on ordinary tool selection,
/// continues the artifact the wrap-up produced.
#[test]
fn a_tool_free_wrap_up_round_keeps_bound_replay_and_the_next_turn_binds_it() -> TestResult {
    let descriptor = test_descriptor(MODEL, "xai-test-route")?;
    let revisions = supergrok_semantic_revisions()?;
    let lower = |request: &ModelRequest, bridge: Option<&ResponsesArtifactReplayBridge>| {
        lower_responses_request(
            request,
            ResponsesRequestConfig::new(
                descriptor.protocol(),
                descriptor.model_family(),
                descriptor.requested_model(),
                SUPERGROK_RESPONSES_DIALECT.request_policy(),
                bridge.map(ResponsesArtifactReplayBridge::context),
            ),
        )
    };
    let first_request = request(vec![user_segment(1)?], 1)?;
    let first_plan = descriptor.validate_request(first_request, ValidationMode::Strict)?;
    let (mut output, mut replay) = canonical_output(&descriptor, 1)?;
    let (refusal, refusal_replay) = refusal_output(&descriptor, 1, 3)?;
    output.push(refusal);
    replay.push(refusal_replay);
    let (first, _) = seal_output(&descriptor, &first_plan, 1, output, replay)?;
    let first_active = active(&first)?;
    let segments = vec![user_segment(1)?, output_segment(&first)?, user_segment(2)?];

    let wrap_up_plan = descriptor.validate_request(
        tool_free_request(segments.clone(), 2)?,
        ValidationMode::Strict,
    )?;
    let wrap_up_bridge = active_replay_bridge(
        Some(&first_active),
        wrap_up_plan.request(),
        authority(&descriptor, &wrap_up_plan, revisions)?,
        now(),
    )?
    .ok_or_else(|| std::io::Error::other("the wrap-up dropped the live artifact"))?;
    let wire = lower(wrap_up_plan.request(), Some(&wrap_up_bridge))?
        .body()
        .to_string();
    for exact in [
        "encrypted-1",
        "future_1",
        "opaque-1",
        "refusal_1",
        "refused 1",
    ] {
        assert!(wire.contains(exact), "wrap-up wire lost {exact}");
    }
    assert_eq!(
        lower(wrap_up_plan.request(), None)
            .map_err(|error| ResponsesAttemptBuildError::Request(error).preparation_error_kind())
            .err(),
        Some(ModelPreparationErrorKind::ProtectedHistory),
        "fresh lowering of this history must stay refused"
    );

    // The wrap-up's own artifact is what the next turn continues.
    let (wrap_up, _) = seal_round(&descriptor, &wrap_up_plan, 2)?;
    let wrap_up_active = active(&wrap_up)?;
    let mut next_segments = segments;
    next_segments.push(output_segment(&wrap_up)?);
    next_segments.push(user_segment(3)?);
    let next_plan =
        descriptor.validate_request(request(next_segments, 3)?, ValidationMode::Strict)?;
    let next_authority = authority(&descriptor, &next_plan, revisions)?;
    assert!(
        active_replay_bridge(
            Some(&first_active),
            next_plan.request(),
            next_authority.clone(),
            now(),
        )?
        .is_none(),
        "the next turn does not continue the superseded artifact"
    );
    let next_bridge = active_replay_bridge(
        Some(&wrap_up_active),
        next_plan.request(),
        next_authority,
        now(),
    )?
    .ok_or_else(|| std::io::Error::other("the next turn dropped the wrap-up artifact"))?;
    let next_wire = lower(next_plan.request(), Some(&next_bridge))?
        .body()
        .to_string();
    for exact in ["encrypted-1", "refused 1", "encrypted-2", "future_2"] {
        assert!(next_wire.contains(exact), "next-turn wire lost {exact}");
    }
    Ok(())
}

/// Any stable-prefix difference other than tool selection prepares fresh.
#[test]
fn a_round_with_other_prefix_drift_prepares_fresh_beside_a_live_artifact() -> TestResult {
    let descriptor = test_descriptor(MODEL, "xai-test-route")?;
    let revisions = supergrok_semantic_revisions()?;
    let first_request = request(vec![user_segment(1)?], 1)?;
    let first_plan = descriptor.validate_request(first_request, ValidationMode::Strict)?;
    let (first, _) = seal_round(&descriptor, &first_plan, 1)?;
    let first_active = active(&first)?;
    let segments = vec![user_segment(1)?, output_segment(&first)?, user_segment(2)?];

    let (selected, effective) = provider_configuration_ids()?;
    let drifted_request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_shared(
            CanonicalHistory::new(segments.clone())?,
            WorkingProjection::default(),
        ),
        metadata(2)?,
    )
    .instructions(vec![plakous_model::InstructionBlock::new(
        plakous_model::InstructionSource::System,
        "answer briefly",
    )?])
    .resolved_harness_plan_identity(harness_plan()?)
    .tool_choice(plakous_model::ToolChoice::None)
    .build()?;
    let drifted_plan = descriptor.validate_request(drifted_request, ValidationMode::Strict)?;
    assert!(
        active_replay_bridge(
            Some(&first_active),
            drifted_plan.request(),
            authority(&descriptor, &drifted_plan, revisions)?,
            now(),
        )?
        .is_none(),
        "an instruction drift must prepare fresh, not refuse"
    );

    // The same history on the live prefix still reuses the artifact:
    // only the instructions made the drifted round a miss.
    let next_plan = descriptor.validate_request(request(segments, 2)?, ValidationMode::Strict)?;
    assert!(
        active_replay_bridge(
            Some(&first_active),
            next_plan.request(),
            authority(&descriptor, &next_plan, revisions)?,
            now(),
        )?
        .is_some()
    );
    Ok(())
}

#[test]
fn active_artifact_refuses_tamper_binding_revision_family_and_missing_authority() -> TestResult {
    let descriptor = test_descriptor(MODEL, "xai-test-route")?;
    let revisions = supergrok_semantic_revisions()?;
    let first_request = request(vec![user_segment(1)?], 1)?;
    let first_plan = descriptor.validate_request(first_request, ValidationMode::Strict)?;
    let (first, producing_binding) = seal_round(&descriptor, &first_plan, 1)?;
    let candidate = first
        .family_artifact_candidate()
        .ok_or_else(|| std::io::Error::other("missing first candidate"))?;
    let second_request = request(
        vec![user_segment(1)?, output_segment(&first)?, user_segment(2)?],
        2,
    )?;
    let second_plan = descriptor.validate_request(second_request, ValidationMode::Strict)?;
    let second_authority = authority(&descriptor, &second_plan, revisions)?;
    assert_eq!(candidate.binding(), producing_binding.identity());
    assert_ne!(
        candidate.binding(),
        second_authority.artifact_binding().identity()
    );
    assert!(
        active_replay_bridge(None, second_plan.request(), second_authority.clone(), now(),)?
            .is_none()
    );

    let invalid = |artifact: ActiveFamilyArtifact| {
        active_replay_bridge(
            Some(&artifact),
            second_plan.request(),
            second_authority.clone(),
            now(),
        )
        .map(|bridge| bridge.is_some())
        .map_err(|error| -> Box<dyn Error> { Box::new(error) })
    };

    let mut tampered = candidate.bytes().to_vec();
    *tampered
        .last_mut()
        .ok_or_else(|| std::io::Error::other("empty candidate bytes"))? ^= 1;
    assert!(
        invalid(ActiveFamilyArtifact::new(
            "responses",
            candidate.schema_revision(),
            candidate.codec_revision(),
            candidate.binding(),
            tampered,
        )?)
        .is_err()
    );
    let wrong_binding = Digest::sha256(
        DigestDomain::ModelBinding,
        1,
        &[b"xai-model-artifact-wrong-wrapper-binding"],
    )?;
    assert!(
        invalid(ActiveFamilyArtifact::new(
            "responses",
            candidate.schema_revision(),
            candidate.codec_revision(),
            wrong_binding,
            candidate.bytes().to_vec(),
        )?)
        .is_err()
    );
    assert!(
        invalid(ActiveFamilyArtifact::new(
            "responses",
            candidate.schema_revision() + 1,
            candidate.codec_revision(),
            candidate.binding(),
            candidate.bytes().to_vec(),
        )?)
        .is_err()
    );
    assert!(
        invalid(ActiveFamilyArtifact::new(
            "messages",
            candidate.schema_revision(),
            candidate.codec_revision(),
            candidate.binding(),
            candidate.bytes().to_vec(),
        )?)
        .is_err()
    );

    let revision_authority = authority(
        &descriptor,
        &second_plan,
        ResponsesSemanticRevisions::new(4, 1, 5)?,
    )?;
    let revision_active = active(&first)?;
    assert!(
        active_replay_bridge(
            Some(&revision_active),
            second_plan.request(),
            revision_authority,
            now(),
        )
        .is_err()
    );
    let foreign_descriptor = test_descriptor("grok-foreign", "xai-test-route")?;
    let foreign_plan = foreign_descriptor
        .validate_request(second_plan.request().clone(), ValidationMode::Strict)?;
    let foreign_active = active(&first)?;
    assert!(
        active_replay_bridge(
            Some(&foreign_active),
            foreign_plan.request(),
            authority(&foreign_descriptor, &foreign_plan, revisions)?,
            now(),
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn tool_output_coverage_is_the_open_prefix_of_the_complete_exchange() -> TestResult {
    let descriptor = test_descriptor(MODEL, "xai-test-route")?;
    let intent = user_segment(1)?;
    let first_request = request(vec![intent.clone()], 1)?;
    let first_plan = descriptor.validate_request(first_request, ValidationMode::Strict)?;
    let correlation = ToolCorrelation::new(
        Some("remote-call-1".to_owned()),
        None,
        plakous_model::ToolCallerLineage::Direct,
    )?;
    let call_id = plakous_model::CallId::from_str("00000000-0000-4000-8000-000000000901")?;
    let call = ToolCallBlock::new(
        call_id,
        ToolName::new("fixture_tool")?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        correlation.clone(),
        r#"{"path":"fixture.txt"}"#,
    )?;
    let call_item = plakous_model::ConversationItem::new(
        EntryId::from_str("00000000-0000-4000-8000-000000000902")?,
        Actor::Assistant,
        vec![ContentBlock::ToolCall(call)],
        Provenance::ModelOutput,
    )?;
    let covered = covered_projection_after_output(&first_plan, std::slice::from_ref(&call_item))?;
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
    let result_item = plakous_model::ConversationItem::new(
        EntryId::from_str("00000000-0000-4000-8000-000000000903")?,
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
