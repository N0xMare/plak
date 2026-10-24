use std::collections::BTreeMap;

use plakous_model::{
    Actor, Capability, CapabilityConstraints, CapabilitySet, ConversationItem, ConversationView,
    EffectiveProviderConfigurationId, EntryId, FactProvenance, FactSource, FeatureSupport,
    OpaqueBlock, OpaqueFallback, Provenance, RequestMetadata, SelectedProviderConfigurationId,
    Support, TextBlock, ValidationMode,
};
use serde_json::value::RawValue;

use super::*;

const SECRET: &str = "encrypted-native-secret-sentinel";

struct Fixture {
    descriptor: ResolvedModelDescriptor,
    producer_request: ModelRequest,
    producer_plan: ValidatedRequestPlan,
    binding: ResponsesArtifactBinding,
    production: ResponsesArtifactProduction,
    contract: ResponsesReplayContract,
    envelope: ReplayEnvelope,
    artifact: ResponsesArtifact,
    next_request: ModelRequest,
    current_binding: ResponsesArtifactBinding,
}

fn digest(domain: DigestDomain, label: &[u8]) -> Digest {
    Digest::sha256(domain, 1, &[label]).expect("valid digest version")
}

fn timestamp(seconds: u64) -> Timestamp {
    Timestamp::new(seconds, 0).expect("timestamp")
}

fn metadata(turn: u128, attempt: u128) -> RequestMetadata {
    RequestMetadata::for_turn(
        "00000000-0000-0000-0000-000000000001"
            .parse::<RunId>()
            .expect("run"),
        format!("00000000-0000-0000-0000-{turn:012x}")
            .parse::<TurnId>()
            .expect("turn"),
        format!("00000000-0000-0000-0000-{attempt:012x}")
            .parse::<AttemptId>()
            .expect("attempt"),
        timestamp(100),
    )
}

fn entry(value: u128) -> EntryId {
    format!("00000000-0000-0000-0000-{value:012x}")
        .parse()
        .expect("entry")
}

fn descriptor(route: &str) -> ResolvedModelDescriptor {
    descriptor_at(route, timestamp(100))
}

fn descriptor_at(route: &str, observed_at: Timestamp) -> ResolvedModelDescriptor {
    let provenance = FactProvenance::new(FactSource::TestFixture, observed_at, None);
    let mut capabilities = BTreeMap::new();
    capabilities.insert(
        Capability::Streaming,
        FeatureSupport::new(
            Support::Native,
            CapabilityConstraints::default(),
            provenance,
        ),
    );
    ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("openai.codex.responses").expect("dialect"),
        RouteId::new(route).expect("route"),
        ModelId::new("gpt-5.6-codex").expect("model"),
        None,
        Some(ModelFamilyId::new("gpt-5.6").expect("family")),
        ModelAdapterProfileId::new("codex.2026-08-r1").expect("adapter"),
        CapabilitySet::new(capabilities, BTreeMap::new()),
    )
}

fn request(input: ConversationView, turn: u128, attempt: u128) -> ModelRequest {
    let selected: SelectedProviderConfigurationId = serde_json::from_value(
        serde_json::to_value(
            Digest::sha256(
                DigestDomain::SelectedProviderConfiguration,
                5,
                &[b"responses-artifact-test-selected-configuration"],
            )
            .expect("selected configuration digest"),
        )
        .expect("selected configuration digest serialization"),
    )
    .expect("selected configuration identity");
    let effective: EffectiveProviderConfigurationId = serde_json::from_value(
        serde_json::to_value(
            Digest::sha256(
                DigestDomain::EffectiveProviderConfiguration,
                5,
                &[b"responses-artifact-test-effective-configuration"],
            )
            .expect("effective configuration digest"),
        )
        .expect("effective configuration digest serialization"),
    )
    .expect("effective configuration identity");
    ModelRequest::builder(selected, effective, input, metadata(turn, attempt))
        .resolved_harness_plan_identity(digest(DigestDomain::ResolvedHarnessPlan, b"tool-plan"))
        .build()
        .expect("request")
}

fn admitted(
    descriptor: &ResolvedModelDescriptor,
    request: &ModelRequest,
    account_label: &[u8],
) -> (ValidatedRequestPlan, ResponsesArtifactBinding) {
    let plan = descriptor
        .validate_request(request.clone(), ValidationMode::Strict)
        .expect("admitted request");
    let binding = ResponsesArtifactBinding::from_admitted_operation(
        descriptor,
        &plan,
        request,
        digest(DigestDomain::ModelBinding, account_label),
        ResponsesSemanticRevisions::new(1, 7, 3).expect("revisions"),
        Some(digest(DigestDomain::CacheAffinity, b"branch-affinity-a")),
    )
    .expect("binding");
    (plan, binding)
}

fn replay_for_model(model: &str, payload: String) -> ReplayEnvelope {
    ReplayEnvelope::new(
        ProtocolId::new("openai.codex.responses").expect("dialect"),
        Some(ModelFamilyId::new("gpt-5.6").expect("family")),
        plakous_model::ReplayScope::ExactAccountModel {
            model: ModelId::new(model).expect("model"),
            account: digest(DigestDomain::ModelBinding, b"account-a"),
        },
        ReplayKindId::new("responses.output_item").expect("kind"),
        1,
        DataClassification::Sensitive,
        RawValue::from_string(payload).expect("raw replay json"),
    )
    .expect("replay")
}

fn replay(payload: String) -> ReplayEnvelope {
    replay_for_model("gpt-5.6-codex", payload)
}

fn contract() -> ResponsesReplayContract {
    ResponsesReplayContract::new(
        ResponsesArtifactReplayKind::new(
            ReplayKindId::new("responses.output_item").expect("kind"),
            1,
        )
        .expect("kind contract"),
    )
}

fn output_item(id: u128, envelope: ReplayEnvelope) -> ConversationItem {
    ConversationItem::new(
        entry(id),
        Actor::Assistant,
        vec![ContentBlock::Opaque(
            OpaqueBlock::new(envelope, OpaqueFallback::CompatibleReplay).expect("opaque"),
        )],
        Provenance::ModelOutput,
    )
    .expect("output item")
}

fn user_item(id: u128, text: &str) -> ConversationItem {
    ConversationItem::new(
        entry(id),
        Actor::User,
        vec![ContentBlock::Text(
            TextBlock::new(text, None).expect("text"),
        )],
        Provenance::UserInput,
    )
    .expect("user item")
}

fn fixture(storage: ResponsesStorageMode, expires_at: Option<Timestamp>) -> Fixture {
    let descriptor = descriptor("codex.subscription");
    let producer_request = request(ConversationView::default(), 2, 3);
    let (producer_plan, binding) = admitted(&descriptor, &producer_request, b"account-a");
    let envelope = replay(r#"{"type":"message","id":"msg_1"}"#.to_owned());
    let output = output_item(10, envelope.clone());
    let next_view = ConversationView::from_items(vec![output, user_item(11, "turn two")])
        .expect("test conversation items form one valid segment");
    let covered = next_view
        .canonical_item_prefix_identity(1)
        .expect("covered prefix")
        .identity();
    let production = ResponsesArtifactProduction::from_admitted_response(
        &producer_request,
        &producer_plan,
        digest(DigestDomain::ProviderOperation, b"physical-request-1"),
        digest(DigestDomain::ProviderOperation, b"terminal-response-1"),
        1,
        covered,
    )
    .expect("production");
    let continuation = matches!(storage, ResponsesStorageMode::ProviderStored).then(|| {
        ResponsesContinuation::new("resp_exact_1", 1, covered, production.terminal_response())
            .expect("continuation")
    });
    let contract = contract();
    let artifact = ResponsesArtifact::new(
        binding.clone(),
        production.clone(),
        contract.clone(),
        vec![ResponsesReplayItem::new(
            ResponsesReplayPurpose::OutputItem,
            0,
            envelope.clone(),
        )],
        continuation,
        None,
        storage,
        expires_at,
        timestamp(100),
    )
    .expect("artifact");
    let next_request = ModelRequest::from_parts(
        producer_request.stable_prefix().clone(),
        next_view,
        metadata(4, 5),
    )
    .expect("next request");
    let (_, current_binding) = admitted(&descriptor, &next_request, b"account-a");
    Fixture {
        descriptor,
        producer_request,
        producer_plan,
        binding,
        production,
        contract,
        envelope,
        artifact,
        next_request,
        current_binding,
    }
}

#[test]
fn turn_one_artifact_codec_turn_two_bridge_selects_one_reuse_path() {
    let stored = fixture(ResponsesStorageMode::ProviderStored, Some(timestamp(200)));
    let bytes = ResponsesArtifactCodec::encode(&stored.artifact).expect("encode");
    let inert = ResponsesArtifactCodec::decode_inert(&bytes).expect("inert decode");
    let active = inert
        .validate_for_reuse(
            &stored.current_binding,
            &stored.next_request,
            timestamp(150),
        )
        .expect("active reuse");
    assert_eq!(
        active.kind(),
        ResponsesArtifactReuseKind::ProviderContinuation
    );
    assert_eq!(
        active.continuation().expect("continuation").response_id(),
        "resp_exact_1"
    );
    assert!(active.replay().is_empty());
    assert!(
        active
            .replay_at(ResponsesReplayPurpose::OutputItem, 0)
            .is_none()
    );

    let local = fixture(ResponsesStorageMode::Stateless, None);
    let active = local
        .artifact
        .validate_for_reuse(&local.current_binding, &local.next_request, timestamp(150))
        .expect("local replay");
    assert_eq!(active.kind(), ResponsesArtifactReuseKind::LocalReplay);
    assert!(active.continuation().is_none());
    assert_eq!(active.replay().len(), 1);
    assert_eq!(
        active
            .replay_at(ResponsesReplayPurpose::OutputItem, 0)
            .expect("coordinate")
            .envelope()
            .digest(),
        local.envelope.digest()
    );
}

#[test]
fn active_artifact_ignores_historical_foreign_replay_but_refuses_replay_tail() {
    let descriptor = descriptor("codex.subscription");
    let historical = output_item(
        40,
        replay_for_model(
            "gpt-5.3-codex-spark",
            r#"{"type":"reasoning","id":"old"}"#.to_owned(),
        ),
    );
    let producer_request = request(
        ConversationView::from_items(vec![historical.clone()])
            .expect("test conversation items form one valid segment"),
        20,
        21,
    );
    let (producer_plan, binding) = admitted(&descriptor, &producer_request, b"account-a");
    let target_envelope = replay(r#"{"type":"function_call","id":"new"}"#.to_owned());
    let target_output = output_item(41, target_envelope.clone());
    let covered_view =
        ConversationView::from_items(vec![historical.clone(), target_output.clone()])
            .expect("test conversation items form one valid segment");
    let covered = covered_view
        .canonical_item_prefix_identity(2)
        .expect("covered prefix")
        .identity();
    let production = ResponsesArtifactProduction::from_admitted_response(
        &producer_request,
        &producer_plan,
        digest(DigestDomain::ProviderOperation, b"mixed-history-request"),
        digest(DigestDomain::ProviderOperation, b"mixed-history-terminal"),
        1,
        covered,
    )
    .expect("production");
    let artifact = ResponsesArtifact::new(
        binding,
        production,
        contract(),
        vec![ResponsesReplayItem::new(
            ResponsesReplayPurpose::OutputItem,
            0,
            target_envelope.clone(),
        )],
        None,
        None,
        ResponsesStorageMode::Stateless,
        None,
        timestamp(100),
    )
    .expect("artifact");
    let next_request = ModelRequest::from_parts(
        producer_request.stable_prefix().clone(),
        ConversationView::from_items(vec![
            historical.clone(),
            target_output.clone(),
            user_item(42, "tool result"),
        ])
        .expect("test conversation items form one valid segment"),
        metadata(22, 23),
    )
    .expect("next request");
    let (_, current) = admitted(&descriptor, &next_request, b"account-a");
    let reuse = artifact
        .validate_for_reuse(&current, &next_request, timestamp(150))
        .expect("historical foreign replay is outside this artifact's authority");
    assert_eq!(reuse.replay().len(), 1);
    let authorized = reuse.replay()[0];
    assert_eq!(authorized.canonical_position(), 1);
    assert_eq!(authorized.envelope().digest(), target_envelope.digest());

    let tail_request = ModelRequest::from_parts(
        producer_request.stable_prefix().clone(),
        ConversationView::from_items(vec![
            historical,
            target_output,
            output_item(
                43,
                replay(r#"{"type":"message","id":"unauthorized"}"#.to_owned()),
            ),
        ])
        .expect("test conversation items form one valid segment"),
        metadata(24, 25),
    )
    .expect("tail request");
    let (_, tail_binding) = admitted(&descriptor, &tail_request, b"account-a");
    assert!(matches!(
        artifact.validate_for_reuse(&tail_binding, &tail_request, timestamp(150)),
        Err(ResponsesArtifactError::UnauthorizedReplayTail { position: 2 })
    ));
}

#[derive(Clone, Copy, Debug)]
enum LongHistoryCorruption {
    Secret,
    PublicEncrypted,
    UnauthorizedTail,
    KindVersion,
    CanonicalDigest,
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one sealed fixture checks late failures after both former history cutoffs"
)]
fn long_history_still_rejects_late_corruption_with_exact_errors() {
    use plakous_model::{CanonicalHistory, HistorySegment, HistorySegmentKind, WorkingProjection};

    let descriptor = descriptor("codex.subscription");
    let historical = replay(
        serde_json::json!({"type":"reasoning", "encrypted_content":"e".repeat(800)}).to_string(),
    );
    let small = replay(r#"{"type":"reasoning","encrypted_content":"latest"}"#.to_owned());
    let items = (0..4_097)
        .map(|index| output_item(10_000 + index, historical.clone()))
        .collect::<Vec<_>>();
    assert!(items.len() > MAX_RESPONSES_REPLAY_ITEMS);
    assert!(items.len() * historical.payload().get().len() > MAX_RESPONSES_REPLAY_BYTES);
    let base = items
        .chunks(2_048)
        .map(|chunk| {
            HistorySegment::new(HistorySegmentKind::ReasoningReplay, chunk.to_vec())
                .expect("segment")
        })
        .collect::<Vec<_>>();
    let view = |segments| {
        ConversationView::from_shared(
            CanonicalHistory::new(segments).expect("history"),
            WorkingProjection::default(),
        )
    };
    let encrypted_kind = ResponsesArtifactReplayKind::new(
        ReplayKindId::new("responses.encrypted_reasoning").expect("kind"),
        1,
    )
    .expect("contract kind");
    for corruption in [
        LongHistoryCorruption::Secret,
        LongHistoryCorruption::PublicEncrypted,
        LongHistoryCorruption::UnauthorizedTail,
        LongHistoryCorruption::KindVersion,
        LongHistoryCorruption::CanonicalDigest,
    ] {
        let mut segments = base.clone();
        let (kind, version, classification) = match corruption {
            LongHistoryCorruption::Secret => {
                ("responses.output_item", 1, DataClassification::Secret)
            }
            LongHistoryCorruption::PublicEncrypted => (
                "responses.encrypted_reasoning",
                1,
                DataClassification::Public,
            ),
            LongHistoryCorruption::KindVersion => {
                ("responses.output_item", 2, DataClassification::Sensitive)
            }
            _ => ("responses.output_item", 1, DataClassification::Sensitive),
        };
        let bad = ReplayEnvelope::new(
            historical.protocol().clone(),
            historical.model_family().cloned(),
            historical.scope().clone(),
            ReplayKindId::new(kind).expect("kind"),
            version,
            classification,
            RawValue::from_string(r#"{"type":"reasoning","encrypted_content":"bad"}"#.to_owned())
                .expect("json"),
        )
        .expect("envelope");
        let bad_segment = HistorySegment::new(
            HistorySegmentKind::ReasoningReplay,
            vec![output_item(20_000, bad)],
        )
        .expect("late segment");
        if !matches!(corruption, LongHistoryCorruption::UnauthorizedTail) {
            segments.push(bad_segment.clone());
        }
        let producing = request(view(segments.clone()), 20, 21);
        let (plan, binding) = admitted(&descriptor, &producing, b"account-a");
        segments.push(
            HistorySegment::new(
                HistorySegmentKind::ReasoningReplay,
                vec![output_item(20_001, small.clone())],
            )
            .expect("latest segment"),
        );
        let covered_view = view(segments.clone());
        let covered = covered_view
            .canonical_item_prefix_identity(covered_view.items().len())
            .expect("coverage")
            .identity();
        let production = ResponsesArtifactProduction::from_admitted_response(
            &producing,
            &plan,
            digest(DigestDomain::ProviderOperation, b"long-negative-request"),
            digest(DigestDomain::ProviderOperation, b"long-negative-terminal"),
            1,
            covered,
        )
        .expect("production");
        let artifact = ResponsesArtifact::new(
            binding,
            production,
            contract()
                .with_encrypted_reasoning(encrypted_kind.clone())
                .expect("encrypted contract"),
            vec![ResponsesReplayItem::new(
                ResponsesReplayPurpose::OutputItem,
                0,
                small.clone(),
            )],
            None,
            None,
            ResponsesStorageMode::Stateless,
            None,
            timestamp(100),
        )
        .expect("valid active artifact");
        artifact
            .validate_active(timestamp(150))
            .expect("artifact integrity");
        if matches!(corruption, LongHistoryCorruption::UnauthorizedTail) {
            segments.push(bad_segment);
        } else if matches!(corruption, LongHistoryCorruption::CanonicalDigest) {
            let late = segments.len() - 2;
            segments[late] = HistorySegment::new(
                HistorySegmentKind::ReasoningReplay,
                vec![output_item(20_000, small.clone())],
            )
            .expect("changed canonical digest");
        }
        let next = request(view(segments), 22, 23);
        let (_, current) = admitted(&descriptor, &next, b"account-a");
        let expected = if matches!(corruption, LongHistoryCorruption::UnauthorizedTail) {
            ResponsesArtifactError::UnauthorizedReplayTail { position: 4_098 }
        } else {
            ResponsesArtifactError::CanonicalCoordinateMismatch
        };
        assert_eq!(
            artifact
                .validate_for_reuse(&current, &next, timestamp(150))
                .err(),
            Some(expected),
            "{corruption:?}"
        );
    }
}

#[test]
fn codec_round_trip_and_golden_are_canonical() {
    let fixture = fixture(ResponsesStorageMode::ProviderStored, Some(timestamp(200)));
    let encoded = ResponsesArtifactCodec::encode(&fixture.artifact).expect("encode");
    let decoded = ResponsesArtifactCodec::decode(
        &encoded,
        &fixture.binding,
        &fixture.production,
        timestamp(150),
    )
    .expect("exact decode");
    assert_eq!(decoded, fixture.artifact);
    assert_eq!(
        decoded.binding().cache_affinity(),
        Some(digest(DigestDomain::CacheAffinity, b"branch-affinity-a"))
    );
    assert_eq!(encoded.len(), fixture.artifact.encoded_len());
    assert_eq!(encoded.len(), 1011);
    assert_eq!(
        *decoded.integrity(),
        [
            0xdb, 0x01, 0x74, 0xb4, 0xc6, 0x34, 0x34, 0x0d, 0xc9, 0x72, 0xf2, 0xc8, 0x24, 0xba,
            0xd0, 0x12, 0xad, 0xaf, 0xfb, 0x9b, 0x00, 0xaf, 0x3f, 0x44, 0x6a, 0x65, 0x07, 0xaa,
            0x93, 0xe1, 0xa0, 0x05,
        ]
    );
}

#[test]
fn binding_identity_covers_cache_affinity_and_rejects_foreign_domain() {
    let descriptor = descriptor("codex.subscription");
    let request = request(ConversationView::default(), 2, 3);
    let plan = descriptor
        .validate_request(request.clone(), ValidationMode::Strict)
        .expect("admitted request");
    let revisions = ResponsesSemanticRevisions::new(1, 7, 3).expect("revisions");
    let account = digest(DigestDomain::ModelBinding, b"account-a");
    let bind = |affinity| {
        ResponsesArtifactBinding::from_admitted_operation(
            &descriptor,
            &plan,
            &request,
            account,
            revisions,
            affinity,
        )
    };
    let first_affinity = digest(DigestDomain::CacheAffinity, b"branch-affinity-a");
    let rotated_affinity = digest(DigestDomain::CacheAffinity, b"branch-affinity-b");
    let first = bind(Some(first_affinity)).expect("first affinity");
    let rotated = bind(Some(rotated_affinity)).expect("rotated affinity");
    let absent = bind(None).expect("absent affinity");

    assert_eq!(first.cache_affinity(), Some(first_affinity));
    assert_ne!(first.identity(), rotated.identity());
    assert_ne!(first.identity(), absent.identity());
    assert!(matches!(
        bind(Some(digest(
            DigestDomain::ModelBinding,
            b"foreign-affinity"
        ))),
        Err(ResponsesArtifactError::InvalidDigest {
            field: "cache affinity"
        })
    ));

    let stored = fixture(ResponsesStorageMode::Stateless, None);
    let current_plan = stored
        .descriptor
        .validate_request(stored.next_request.clone(), ValidationMode::Strict)
        .expect("current admitted request");
    let rotated_current = ResponsesArtifactBinding::from_admitted_operation(
        &stored.descriptor,
        &current_plan,
        &stored.next_request,
        account,
        revisions,
        Some(rotated_affinity),
    )
    .expect("rotated current binding");
    assert_eq!(
        stored
            .artifact
            .validate_for_reuse(&rotated_current, &stored.next_request, timestamp(150))
            .map(|_| ()),
        Err(ResponsesArtifactError::BindingMismatch)
    );
}

#[test]
fn reusable_target_ignores_descriptor_observation_freshness() {
    let original = descriptor_at("codex.subscription", timestamp(100));
    let refreshed = descriptor_at("codex.subscription", timestamp(200));
    assert_ne!(
        original.binding_digest().expect("original exact binding"),
        refreshed.binding_digest().expect("refreshed exact binding")
    );
    assert_eq!(
        original
            .semantic_binding_digest()
            .expect("original semantic binding"),
        refreshed
            .semantic_binding_digest()
            .expect("refreshed semantic binding")
    );

    let request = request(ConversationView::default(), 2, 3);
    let (_, original_binding) = admitted(&original, &request, b"account-a");
    let (_, refreshed_binding) = admitted(&refreshed, &request, b"account-a");
    assert!(original_binding.same_reuse_target(&refreshed_binding));
}

#[test]
fn expired_history_decodes_inert_but_cannot_be_reused() {
    let fixture = fixture(ResponsesStorageMode::ProviderStored, Some(timestamp(200)));
    let encoded = ResponsesArtifactCodec::encode(&fixture.artifact).expect("encode");
    let decoded = ResponsesArtifactCodec::decode_inert(&encoded).expect("historical decode");
    decoded.validate_integrity().expect("structural integrity");
    assert!(decoded.is_expired(timestamp(200)));
    assert_eq!(
        decoded.validate_for(&fixture.binding, &fixture.production, timestamp(200)),
        Err(ResponsesArtifactError::Expired)
    );
    assert_eq!(
        decoded
            .validate_for_reuse(
                &fixture.current_binding,
                &fixture.next_request,
                timestamp(200)
            )
            .map(|_| ()),
        Err(ResponsesArtifactError::Expired)
    );
}

#[test]
fn cross_route_account_attempt_and_physical_producer_fail_closed() {
    let fixture = fixture(ResponsesStorageMode::Stateless, None);
    let other_route = descriptor("other.route");
    let (_, route_binding) = admitted(&other_route, &fixture.next_request, b"account-a");
    assert_eq!(
        fixture
            .artifact
            .validate_for_reuse(&route_binding, &fixture.next_request, timestamp(150))
            .map(|_| ()),
        Err(ResponsesArtifactError::BindingMismatch)
    );
    let (_, account_binding) = admitted(&fixture.descriptor, &fixture.next_request, b"account-b");
    assert_eq!(
        fixture
            .artifact
            .validate_for_reuse(&account_binding, &fixture.next_request, timestamp(150))
            .map(|_| ()),
        Err(ResponsesArtifactError::BindingMismatch)
    );

    let different_attempt = fixture
        .producer_request
        .for_round(metadata(2, 99))
        .expect("other attempt");
    let (different_plan, attempt_binding) =
        admitted(&fixture.descriptor, &different_attempt, b"account-a");
    assert_eq!(
        fixture
            .artifact
            .validate_for(&attempt_binding, &fixture.production, timestamp(150)),
        Err(ResponsesArtifactError::BindingMismatch)
    );

    let mut physical = fixture.production.clone();
    physical.physical_request = digest(DigestDomain::ProviderOperation, b"other-physical");
    assert_eq!(
        fixture
            .artifact
            .validate_for(&fixture.binding, &physical, timestamp(150)),
        Err(ResponsesArtifactError::BindingMismatch)
    );

    let transplanted = ResponsesArtifactProduction::from_admitted_response(
        &different_attempt,
        &different_plan,
        digest(DigestDomain::ProviderOperation, b"other-physical"),
        digest(DigestDomain::ProviderOperation, b"other-terminal"),
        1,
        fixture.production.covered_projection(),
    )
    .expect("independently valid foreign production");
    assert_eq!(
        ResponsesArtifact::new(
            fixture.binding.clone(),
            transplanted,
            fixture.contract.clone(),
            vec![ResponsesReplayItem::new(
                ResponsesReplayPurpose::OutputItem,
                0,
                fixture.envelope.clone(),
            )],
            None,
            None,
            ResponsesStorageMode::Stateless,
            None,
            timestamp(150),
        ),
        Err(ResponsesArtifactError::ProductionBindingMismatch)
    );
}

#[test]
fn coordinate_transplant_and_request_projection_drift_fail_closed() {
    let fixture = fixture(ResponsesStorageMode::Stateless, None);
    let drifted_view = ConversationView::from_items(vec![
        user_item(20, "foreign prefix"),
        output_item(21, fixture.envelope.clone()),
    ])
    .expect("test conversation items form one valid segment");
    let drifted_request = ModelRequest::from_parts(
        fixture.producer_request.stable_prefix().clone(),
        drifted_view,
        metadata(4, 6),
    )
    .expect("drifted request");
    let (_, drifted_binding) = admitted(&fixture.descriptor, &drifted_request, b"account-a");
    assert_eq!(
        fixture
            .artifact
            .validate_for_reuse(&drifted_binding, &drifted_request, timestamp(150))
            .map(|_| ()),
        Err(ResponsesArtifactError::CanonicalCoordinateMismatch)
    );
}

#[test]
fn purpose_kind_mislabeling_and_ambiguous_contract_are_rejected() {
    let fixture = fixture(ResponsesStorageMode::Stateless, None);
    assert_eq!(
        ResponsesArtifact::new(
            fixture.binding.clone(),
            fixture.production.clone(),
            fixture.contract.clone(),
            vec![ResponsesReplayItem::new(
                ResponsesReplayPurpose::EncryptedReasoning,
                0,
                fixture.envelope.clone()
            )],
            None,
            None,
            ResponsesStorageMode::Stateless,
            None,
            timestamp(100)
        ),
        Err(ResponsesArtifactError::ReplayPurposeKindMismatch { index: 0 })
    );
    let same = ResponsesArtifactReplayKind::new(
        ReplayKindId::new("responses.output_item").expect("kind"),
        2,
    )
    .expect("kind pin");
    assert_eq!(
        contract().with_encrypted_reasoning(same),
        Err(ResponsesArtifactError::AmbiguousReplayContract)
    );

    let secret = ReplayEnvelope::new(
        fixture.binding.dialect().clone(),
        fixture.binding.model_family().cloned(),
        plakous_model::ReplayScope::ExactAccountModel {
            model: fixture.binding.model().clone(),
            account: fixture.binding.account(),
        },
        ReplayKindId::new("responses.output_item").expect("kind"),
        1,
        DataClassification::Secret,
        RawValue::from_string(r#"{"credential":"forbidden"}"#.to_owned()).expect("raw json"),
    )
    .expect("secret envelope");
    assert_eq!(
        ResponsesArtifact::new(
            fixture.binding,
            fixture.production,
            fixture.contract,
            vec![ResponsesReplayItem::new(
                ResponsesReplayPurpose::OutputItem,
                0,
                secret
            )],
            None,
            None,
            ResponsesStorageMode::Stateless,
            None,
            timestamp(100)
        ),
        Err(ResponsesArtifactError::SecretReplayMaterial { index: 0 })
    );
}

#[test]
fn continuation_and_cache_coverage_must_be_exact() {
    let fixture = fixture(ResponsesStorageMode::ProviderStored, None);
    let foreign = digest(DigestDomain::CanonicalItemPrefix, b"foreign-prefix");
    let continuation = ResponsesContinuation::new(
        "resp_1",
        fixture.production.covered_projection_items(),
        foreign,
        fixture.production.terminal_response(),
    )
    .expect("structural continuation");
    assert_eq!(
        ResponsesArtifact::new(
            fixture.binding.clone(),
            fixture.production.clone(),
            fixture.contract.clone(),
            vec![ResponsesReplayItem::new(
                ResponsesReplayPurpose::OutputItem,
                0,
                fixture.envelope.clone(),
            )],
            Some(continuation),
            None,
            ResponsesStorageMode::ProviderStored,
            None,
            timestamp(100)
        ),
        Err(ResponsesArtifactError::ContinuationCoverageMismatch)
    );
    let cache = ResponsesCacheFact::new(
        "cache_1",
        fixture.binding.stable_prefix(),
        fixture.production.covered_projection_items(),
        foreign,
    )
    .expect("structural cache");
    assert_eq!(
        ResponsesArtifact::new(
            fixture.binding,
            fixture.production,
            fixture.contract,
            vec![ResponsesReplayItem::new(
                ResponsesReplayPurpose::OutputItem,
                0,
                fixture.envelope.clone(),
            )],
            fixture.artifact.continuation,
            Some(cache),
            ResponsesStorageMode::ProviderStored,
            None,
            timestamp(100)
        ),
        Err(ResponsesArtifactError::CacheCoverageMismatch)
    );
}

#[test]
fn count_and_aggregate_replay_bounds_fail_before_body_amplification() {
    let fixture = fixture(ResponsesStorageMode::Stateless, None);
    let item = ResponsesReplayItem::new(
        ResponsesReplayPurpose::OutputItem,
        0,
        fixture.envelope.clone(),
    );
    let count = u32::try_from(MAX_RESPONSES_REPLAY_ITEMS + 1).expect("count");
    let production = ResponsesArtifactProduction::from_parts(
        fixture.production.producer_request(),
        fixture.production.producing_projection(),
        fixture.production.validated_plan(),
        fixture.production.physical_request(),
        fixture.production.terminal_response(),
        0,
        count,
        count,
        digest(DigestDomain::CanonicalItemPrefix, b"large-covered"),
    )
    .expect("production");
    assert_eq!(
        ResponsesArtifact::new(
            fixture.binding.clone(),
            production,
            fixture.contract.clone(),
            vec![item; MAX_RESPONSES_REPLAY_ITEMS + 1],
            None,
            None,
            ResponsesStorageMode::Stateless,
            None,
            timestamp(100)
        ),
        Err(ResponsesArtifactError::TooManyReplayItems {
            maximum: MAX_RESPONSES_REPLAY_ITEMS
        })
    );

    let output_count = 4_u32;
    let production = ResponsesArtifactProduction::from_parts(
        fixture.production.producer_request(),
        fixture.production.producing_projection(),
        fixture.production.validated_plan(),
        fixture.production.physical_request(),
        fixture.production.terminal_response(),
        0,
        output_count,
        output_count,
        digest(DigestDomain::CanonicalItemPrefix, b"aggregate-covered"),
    )
    .expect("production");
    let large = format!(r#"{{"opaque":"{}"}}"#, "x".repeat(800_000));
    let replay = (0..output_count)
        .map(|index| {
            ResponsesReplayItem::new(
                ResponsesReplayPurpose::OutputItem,
                index,
                replay(large.clone()),
            )
        })
        .collect();
    assert_eq!(
        ResponsesArtifact::new(
            fixture.binding,
            production,
            fixture.contract,
            replay,
            None,
            None,
            ResponsesStorageMode::Stateless,
            None,
            timestamp(100)
        ),
        Err(ResponsesArtifactError::ReplayBytesTooLarge {
            maximum: MAX_RESPONSES_REPLAY_BYTES
        })
    );
}

#[test]
fn identical_envelopes_remain_valid_at_distinct_exact_coordinates() {
    let fixture = fixture(ResponsesStorageMode::Stateless, None);
    let view = ConversationView::from_items(vec![
        output_item(30, fixture.envelope.clone()),
        output_item(31, fixture.envelope.clone()),
    ])
    .expect("test conversation items form one valid segment");
    let covered = view
        .canonical_item_prefix_identity(2)
        .expect("covered")
        .identity();
    let production = ResponsesArtifactProduction::from_admitted_response(
        &fixture.producer_request,
        &fixture.producer_plan,
        fixture.production.physical_request(),
        fixture.production.terminal_response(),
        2,
        covered,
    )
    .expect("production");
    let artifact = ResponsesArtifact::new(
        fixture.binding,
        production,
        fixture.contract,
        vec![
            ResponsesReplayItem::new(
                ResponsesReplayPurpose::OutputItem,
                0,
                fixture.envelope.clone(),
            ),
            ResponsesReplayItem::new(
                ResponsesReplayPurpose::OutputItem,
                1,
                fixture.envelope.clone(),
            ),
        ],
        None,
        None,
        ResponsesStorageMode::Stateless,
        None,
        timestamp(100),
    )
    .expect("artifact");
    let next_request = ModelRequest::from_parts(
        fixture.producer_request.stable_prefix().clone(),
        view,
        metadata(4, 7),
    )
    .expect("request");
    let (_, current) = admitted(&fixture.descriptor, &next_request, b"account-a");
    let active = artifact
        .validate_for_reuse(&current, &next_request, timestamp(150))
        .expect("active replay");
    let first = active
        .replay_at(ResponsesReplayPurpose::OutputItem, 0)
        .expect("first");
    let second = active
        .replay_at(ResponsesReplayPurpose::OutputItem, 1)
        .expect("second");
    assert_eq!(first.envelope().digest(), second.envelope().digest());
    assert_ne!(first.canonical_position(), second.canonical_position());
}

#[test]
fn corruption_unknown_revision_and_outer_bound_are_rejected() {
    let fixture = fixture(ResponsesStorageMode::Stateless, None);
    let encoded = ResponsesArtifactCodec::encode(&fixture.artifact).expect("encode");
    let mut corrupt = encoded.clone();
    corrupt[ENVELOPE_OVERHEAD] ^= 1;
    assert_eq!(
        ResponsesArtifactCodec::decode_inert(&corrupt),
        Err(ResponsesArtifactError::IntegrityMismatch)
    );
    let mut unknown = encoded;
    unknown[MAGIC.len()..MAGIC.len() + 4].copy_from_slice(&99_u32.to_be_bytes());
    assert_eq!(
        ResponsesArtifactCodec::decode_inert(&unknown),
        Err(ResponsesArtifactError::UnsupportedCodecRevision { found: 99 })
    );
    assert_eq!(
        ResponsesArtifactCodec::decode_inert(&vec![0; MAX_RESPONSES_ARTIFACT_BYTES + 1]),
        Err(ResponsesArtifactError::ArtifactTooLarge {
            maximum: MAX_RESPONSES_ARTIFACT_BYTES
        })
    );
}

#[test]
fn sensitive_native_values_are_redacted_from_debug() {
    let fixture = fixture(ResponsesStorageMode::ProviderStored, None);
    let sensitive = replay(format!(
        r#"{{"type":"reasoning","encrypted_content":"{SECRET}"}}"#
    ));
    let artifact = ResponsesArtifact::new(
        fixture.binding,
        fixture.production,
        fixture.contract,
        vec![ResponsesReplayItem::new(
            ResponsesReplayPurpose::OutputItem,
            0,
            sensitive,
        )],
        fixture.artifact.continuation,
        None,
        ResponsesStorageMode::ProviderStored,
        None,
        timestamp(100),
    )
    .expect("payload-free artifact");
    let debug = format!("{artifact:?}");
    assert!(!debug.contains(SECRET));
    assert_eq!(artifact.replay_bytes(), 0);
    assert!(
        !String::from_utf8_lossy(&ResponsesArtifactCodec::encode(&artifact).expect("encode"))
            .contains(SECRET)
    );
    assert!(!format!("{:?}", artifact.continuation()).contains("resp_exact_1"));
}
