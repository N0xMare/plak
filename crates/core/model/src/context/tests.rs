use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use proptest::prelude::*;
use uuid::Uuid;

use super::*;
use crate::{
    Actor, AttemptId, CanonicalHistory, Capability, CapabilityConstraints, CapabilitySet,
    ContentBlock, ConversationItem, ConversationView, EffectiveProviderConfigurationId, EntryId,
    FactProvenance, FactSource, FeatureSupport, GenerationLimits, HistorySegment,
    HistorySegmentKind, LanguageModel, MAX_PREPARED_CALL_RECORD_BYTES, ModelError, ModelErrorKind,
    ModelPreparationContext, ModelPreparationError, ModelStream, PreparedCacheUse,
    PreparedCallRecord, PreparedModelCall, PreparedModelProjection, PreparedOutputLimit,
    PreparedTokenProjection, Provenance, RequestMetadata, ResolvedModelDescriptor, RunId,
    RuntimeModelAuthority, SelectedProviderConfigurationId, Support, TextBlock, TurnId,
    ValidatedRequestPlan, ValidationMode, WorkingProjection,
};

fn timestamp(seconds: u64) -> Timestamp {
    Timestamp::new(seconds, 0).expect("valid timestamp")
}

fn authority(value: &'static [u8]) -> Digest {
    Digest::sha256(DigestDomain::ModelBinding, 7, &[value]).expect("authority digest")
}

fn provider_configuration_ids() -> (
    SelectedProviderConfigurationId,
    EffectiveProviderConfigurationId,
) {
    let selected = Digest::sha256(
        DigestDomain::SelectedProviderConfiguration,
        5,
        &[b"context-test-selected"],
    )
    .expect("selected digest");
    let effective = Digest::sha256(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        &[b"context-test-effective"],
    )
    .expect("effective digest");
    (
        SelectedProviderConfigurationId::new(selected).expect("selected identity"),
        EffectiveProviderConfigurationId::new(effective).expect("effective identity"),
    )
}

fn credential_authority() -> Digest {
    authority(b"credential-authority")
}

fn authority_class(account: Digest) -> Digest {
    RuntimeModelAuthority::authority_class_for(account, credential_authority())
        .expect("authority class")
}

fn request(max_output: Option<u32>) -> ModelRequest {
    request_with_input(ConversationView::default(), max_output)
}

fn request_with_input(input: ConversationView, max_output: Option<u32>) -> ModelRequest {
    let limits = GenerationLimits::new(max_output.and_then(NonZeroU32::new), Vec::new())
        .expect("generation limits");
    let (selected, effective) = provider_configuration_ids();
    ModelRequest::builder(
        selected,
        effective,
        input,
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(1)).expect("run ID"),
            TurnId::try_from(Uuid::from_u128(2)).expect("turn ID"),
            AttemptId::try_from(Uuid::from_u128(3)).expect("attempt ID"),
            timestamp(1),
        ),
    )
    .limits(limits)
    .build()
    .expect("request")
}

fn text_item(value: u128, actor: Actor, text: &str) -> ConversationItem {
    ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(value)).expect("entry ID"),
        actor,
        vec![ContentBlock::Text(
            TextBlock::new(text, None).expect("text"),
        )],
        if actor == Actor::User {
            Provenance::UserInput
        } else {
            Provenance::ModelOutput
        },
    )
    .expect("item")
}

fn measurement_certificate(kind: CountCertificateKind) -> MeasurementCertificate {
    let source = if matches!(kind, CountCertificateKind::ReleaseVerifiedUpperBound { .. }) {
        ContextEvidenceSource::ReleaseCertificate
    } else {
        ContextEvidenceSource::BuiltInCatalog
    };
    let freshness = EvidenceFreshness::new(timestamp(1), Some(timestamp(100))).expect("freshness");
    if matches!(kind, CountCertificateKind::CustomEnvelopeUpperBound { .. }) {
        MeasurementCertificate::new(
            "fixture-counter",
            "cert-1",
            "complete-projection-counter",
            "method-1",
            source,
            kind,
            None,
            freshness,
        )
    } else {
        MeasurementCertificate::standard(
            &StandardIntegrationAuthority::release_owned(),
            "fixture-counter",
            "cert-1",
            "complete-projection-counter",
            "method-1",
            source,
            kind,
            None,
            freshness,
        )
    }
    .expect("measurement certificate")
}

fn contract(rule: CapacityRule, certificate: MeasurementCertificate) -> ContextContract {
    ContextContract::new(
        ProtocolId::new("responses-family").expect("protocol family"),
        "accounting-1",
        "projection-1",
        "reservation-1",
        CapacityCertificate::new(
            &StandardIntegrationAuthority::release_owned(),
            "fixture-capacity",
            "capacity-1",
            ContextEvidenceSource::BuiltInCatalog,
            rule,
            None,
            EvidenceFreshness::new(timestamp(1), Some(timestamp(100))).expect("freshness"),
        )
        .expect("capacity certificate"),
        vec![certificate],
    )
    .expect("context contract")
}

fn descriptor(rule: CapacityRule, certificate: MeasurementCertificate) -> ResolvedModelDescriptor {
    let provenance = FactProvenance::new(FactSource::TestFixture, timestamp(1), None);
    let mut features = BTreeMap::new();
    features.insert(
        Capability::Streaming,
        FeatureSupport::new(
            Support::Native,
            CapabilityConstraints::default(),
            provenance,
        ),
    );
    ResolvedModelDescriptor::new(
        &StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("test-dialect").expect("protocol"),
        RouteId::new("test-route").expect("route"),
        ModelId::new("requested-model").expect("model"),
        Some(ModelId::new("effective-model").expect("reported model")),
        None,
        ModelAdapterProfileId::new("test-adapter").expect("adapter"),
        CapabilitySet::new(features, BTreeMap::new()),
    )
    .with_context_contract(contract(rule, certificate))
}

fn observation(tokens: u64) -> CountObservation {
    CountObservation::new("fixture-counter", "cert-1", tokens).expect("observation")
}

fn prepared(
    descriptor: &ResolvedModelDescriptor,
    request: &ModelRequest,
    account: Digest,
    observation: Option<CountObservation>,
) -> (ValidatedRequestPlan, PreparedModelProjection) {
    let plan = descriptor
        .validate_request(request.clone(), ValidationMode::Strict)
        .expect("validated plan");
    let authority = RuntimeModelAuthority::new(
        descriptor,
        account,
        credential_authority(),
        1,
        timestamp(1),
        Some(timestamp(100)),
    )
    .expect("runtime authority");
    let context = ModelPreparationContext::new(authority, timestamp(2));
    let wire = serde_json::to_vec(&request.model_input_identity()).expect("wire");
    let wire_digest = Digest::sha256(DigestDomain::Blob, 1, &[b"context-test-wire-v1", &wire])
        .expect("wire digest");
    let wire_size = u64::try_from(wire.len()).expect("wire size");
    let token_projection = PreparedTokenProjection::new(wire_digest, wire_size, observation)
        .expect("token projection");
    let output_limit = request.limits().max_output_tokens().map(|tokens| {
        PreparedOutputLimit::enforced_wire_maximum(
            tokens,
            "max_output_tokens",
            wire_digest,
            "test-lowering-1",
        )
        .expect("output limit")
    });
    let contract = descriptor.context_contract();
    let protocol_family = contract.map_or_else(
        || descriptor.protocol().clone(),
        |value| value.protocol_family().clone(),
    );
    let projection_revision = contract.map_or("projection-1", ContextContract::projection_revision);
    let accounting_revision = contract.map_or("accounting-1", ContextContract::accounting_revision);
    let projection = PreparedModelProjection::new(
        descriptor,
        &plan,
        &context,
        protocol_family,
        "test-lowering-1",
        projection_revision,
        accounting_revision,
        wire_digest,
        wire_size,
        token_projection,
        PreparedCacheUse::None,
        output_limit,
    )
    .expect("prepared projection");
    (plan, projection)
}

fn item_status(
    input: &ContextWireInput,
    anchor: Option<&ContextRequestAnchor>,
    binding: Digest,
    require_bound: bool,
) -> ContextStatus {
    let descriptor = descriptor(
        CapacityRule::SharedWindow {
            total: SafeLimit::Exact(1_000_000),
        },
        measurement_certificate(CountCertificateKind::ExactTokenizer),
    );
    let request = request(Some(128_000));
    prepared_status(
        &descriptor,
        &request,
        authority(b"account"),
        Some(observation(10)),
        timestamp(2),
    )
    .evaluate_wire_input(&request, input, anchor, binding, require_bound)
    .expect("item status")
}

fn wire_input(value: &serde_json::Value) -> ContextWireInput {
    ContextWireInput::measure_wire(
        &serde_json::to_vec(value).expect("wire"),
        Some(CONSERVATIVE_IMAGE_MAX_TOKENS),
    )
    .expect("item input")
}

#[test]
fn anchor_requires_positive_input_but_allows_zero_output() {
    let input = wire_input(&serde_json::json!({"messages": []}));
    let make = |usage| {
        ContextRequestAnchor::new(
            authority(b"binding"),
            RouteId::new("test-route").expect("route"),
            ModelId::new("requested-model").expect("model"),
            usage,
            input.clone(),
        )
    };
    assert!(make(crate::Usage::new(0, 100, 0, 0, 0)).is_none());
    assert!(make(crate::Usage::new(1, 0, 0, 0, 0)).is_some());
}

#[test]
fn completed_response_headroom_requires_the_item_admission_binding() {
    let input = wire_input(&serde_json::json!({"messages": []}));
    let status = item_status(&input, None, authority(b"binding"), false);
    let mut anchor = ContextRequestAnchor::new(
        authority(b"binding"),
        RouteId::new("test-route").expect("route"),
        ModelId::new("requested-model").expect("model"),
        crate::Usage::new(3000, 100, 0, 0, 0),
        input,
    )
    .expect("anchor");
    assert!(status.completed_response_headroom(&anchor).is_some());
    anchor.binding = authority(b"another-binding");
    assert!(status.completed_response_headroom(&anchor).is_none());
}

#[test]
fn anchored_bound_uses_the_tighter_wire_bound_and_small_envelope_margin() {
    let input = wire_input(&serde_json::json!({"messages": [{"role":"user","content":"hello"}]}));
    let binding = authority(b"binding");
    let anchor = ContextRequestAnchor::new(
        binding,
        RouteId::new("test-route").expect("route"),
        ModelId::new("requested-model").expect("model"),
        crate::Usage::new(900_000, 0, 0, 0, 0),
        input.clone(),
    )
    .expect("anchor");
    let status = item_status(&input, Some(&anchor), binding, true);
    assert_eq!(
        status.local_input_upper_bound(),
        Some(input.upper_bound() + context_admission_margin(1_000_000))
    );
    assert_eq!(context_admission_margin(2048), 204);
}

#[test]
fn only_a_declared_same_served_model_anchors_item_admission() {
    let input = wire_input(&serde_json::json!({
        "messages": [{"role": "user", "content": "x".repeat(20_000)}]
    }));
    let declared = |served: &[&str]| {
        descriptor(
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(1_000_000),
            },
            measurement_certificate(CountCertificateKind::ExactTokenizer),
        )
        .with_same_served_models(
            served
                .iter()
                .map(|model| ModelId::new(*model).expect("model")),
        )
    };
    let measure = |descriptor: &ResolvedModelDescriptor, served: &str| {
        let binding = descriptor.semantic_binding_digest().expect("binding");
        let anchor = ContextRequestAnchor::new(
            binding,
            RouteId::new("test-route").expect("route"),
            ModelId::new(served).expect("model"),
            crate::Usage::new(1_000, 0, 0, 0, 0),
            input.clone(),
        )
        .expect("anchor");
        let request = request(Some(128_000));
        let status = prepared_status(
            descriptor,
            &request,
            authority(b"account"),
            Some(observation(10)),
            timestamp(2),
        )
        .evaluate_wire_input(&request, &input, Some(&anchor), binding, true)
        .expect("item status");
        (
            anchor.matches_descriptor(descriptor),
            status.completed_response_headroom(&anchor).is_some(),
            status.local_input_upper_bound(),
        )
    };
    let margin = context_admission_margin(1_000_000);
    let anchored = Some(1_000 + margin);
    let whole_wire = Some(input.upper_bound() + margin);
    assert!(anchored < whole_wire);

    let variant = declared(&["served-variant"]);
    assert_eq!(measure(&variant, "served-variant"), (true, true, anchored));
    assert_eq!(measure(&variant, "requested-model"), (true, true, anchored));
    assert_eq!(measure(&variant, "effective-model"), (true, true, anchored));
    assert_eq!(
        measure(&variant, "undeclared-variant"),
        (false, false, whole_wire)
    );
    assert_eq!(
        measure(&declared(&[]), "served-variant"),
        (false, false, whole_wire)
    );
}

#[test]
fn messages_cache_breakpoint_does_not_reprice_the_previous_result() {
    let old = wire_input(
        &serde_json::json!({"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"x".repeat(10_000),"cache_control":{"type":"ephemeral"}}]}]}),
    );
    let new = wire_input(
        &serde_json::json!({"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"x".repeat(10_000)}]}, {"role":"assistant","content":[{"type":"text","text":"next","cache_control":{"type":"ephemeral"}}]}]}),
    );
    let anchor = ContextRequestAnchor::new(
        authority(b"binding"),
        RouteId::new("test-route").expect("route"),
        ModelId::new("requested-model").expect("model"),
        crate::Usage::new(3000, 100, 0, 0, 0),
        old,
    )
    .expect("anchor");
    assert_eq!(anchor.occupied_tokens(&new), 3100);
    let data = |value| {
        wire_input(
            &serde_json::json!({"messages":[{"role":"user","content":[{"type":"tool_result","content":{"cache_control":value}}]}]}),
        )
    };
    assert_ne!(
        data("first").items[0].digest,
        data("second").items[0].digest
    );
}

#[test]
fn large_item_anchor_encoding_is_compact_and_tool_arguments_are_not_images() {
    let messages = (0..6000)
        .map(|i| serde_json::json!({"role":"user", "content": i.to_string()}))
        .collect::<Vec<_>>();
    let input = wire_input(&serde_json::json!({"messages":messages}));
    let anchor = ContextRequestAnchor::new(
        authority(b"binding"),
        RouteId::new("test-route").expect("route"),
        ModelId::new("requested-model").expect("model"),
        crate::Usage::new(30_000, 0, 0, 0, 0),
        input.clone(),
    )
    .expect("anchor");
    let encoded = serde_json::to_vec(&anchor).expect("encode");
    assert!(encoded.len() < 1024 * 1024, "{} bytes", encoded.len());
    assert_eq!(
        serde_json::from_slice::<ContextRequestAnchor>(&encoded).expect("restore"),
        anchor
    );
    let arguments = serde_json::json!({"messages":[{"role":"assistant","content":[{"type":"tool_use","input":{"type":"image","payload":"x".repeat(10_000)}}]}]});
    let measured =
        ContextWireInput::measure_wire(&serde_json::to_vec(&arguments).expect("json"), None)
            .expect("not an image block");
    assert!(measured.upper_bound() > 10_000);
}

#[test]
fn prepared_record_with_six_thousand_items_retains_only_an_anchor_reference() {
    let descriptor = descriptor(
        CapacityRule::SharedWindow {
            total: SafeLimit::Exact(1_000_000),
        },
        measurement_certificate(CountCertificateKind::ExactTokenizer),
    );
    let request = request(Some(128_000));
    let (plan, _) = prepared(
        &descriptor,
        &request,
        authority(b"account"),
        Some(observation(10)),
    );
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        authority(b"account"),
        credential_authority(),
        1,
        timestamp(1),
        Some(timestamp(100)),
    )
    .expect("authority");
    let messages = (0..6000)
        .map(|i| serde_json::json!({"role":"user","content":i.to_string()}))
        .collect::<Vec<_>>();
    let body = serde_json::to_vec(&serde_json::json!({"messages":messages})).expect("body");
    let input = ContextWireInput::measure_wire(&body, None).expect("input");
    let anchor = ContextRequestAnchor::new(
        descriptor.semantic_binding_digest().expect("binding"),
        descriptor.route().clone(),
        descriptor.requested_model().clone(),
        crate::Usage::new(30_000, 0, 0, 0, 0),
        input,
    )
    .expect("anchor");
    let context =
        ModelPreparationContext::new(authority, timestamp(2)).with_request_anchor(Some(anchor));
    let digest = Digest::sha256(DigestDomain::Blob, 1, &[&body]).expect("wire digest");
    let size = u64::try_from(body.len()).expect("size");
    let tokens = PreparedTokenProjection::new(digest, size, Some(observation(10)))
        .expect("tokens")
        .with_wire_input(&body, None, &context)
        .expect("items");
    let projection = PreparedModelProjection::new(
        &descriptor,
        &plan,
        &context,
        descriptor
            .context_contract()
            .expect("contract")
            .protocol_family()
            .clone(),
        "test-lowering-1",
        "projection-1",
        "accounting-1",
        digest,
        size,
        tokens,
        PreparedCacheUse::None,
        Some(
            PreparedOutputLimit::enforced_wire_maximum(
                NonZeroU32::new(128_000).expect("output"),
                "max_output_tokens",
                digest,
                "test-lowering-1",
            )
            .expect("limit"),
        ),
    )
    .expect("projection");
    let record = PreparedCallRecord::for_prepared(&descriptor, &plan, projection, &context)
        .expect("prepared record");
    let encoded = serde_json::to_vec(&record).expect("encode");
    assert!(encoded.len() < 1024 * 1024, "{} bytes", encoded.len());
    let restored = serde_json::from_slice::<PreparedCallRecord>(&encoded).expect("restore");
    assert_eq!(restored, record);
    assert_eq!(serde_json::to_vec(&restored).expect("reencode"), encoded);
    assert!(
        !String::from_utf8(encoded)
            .expect("utf8")
            .contains("\"request_anchor\":")
    );
    let oversized = ModelError::from(ModelPreparationError::from(
        crate::PreparedCallError::RecordTooLarge,
    ));
    assert_eq!(oversized.kind(), ModelErrorKind::ContextLimit);
}

#[test]
fn small_ceiling_trigger_caps_the_floor_and_catalog_limit_at_round_headroom() {
    let rule = CapacityRule::ProviderReservedInput {
        reported_context_window: Some(20_000),
        maximum_projected_input: 20_000,
        auto_compact_input: Some(19_000),
    };
    assert_eq!(rule.compaction_trigger_tokens(0, 6000, 8192), 15_904);
    assert_eq!(rule.compaction_trigger_tokens(0, 1000, 1000), 15_904);
    let rule = CapacityRule::SharedWindow {
        total: SafeLimit::Exact(40_000),
    };
    assert_eq!(rule.compaction_trigger_tokens(0, 20_000, 8192), 32_288);
    assert_eq!(rule.compaction_trigger_tokens(4096, 20_000, 8192), 31_808);
}

#[test]
fn item_delta_anchor_counts_dynamic_system_changes_but_not_old_input_or_response_twice() {
    let binding = authority(b"binding");
    let huge = "x".repeat(850_715 * 4);
    let old = wire_input(
        &serde_json::json!({"instructions":"clock=1", "input":[{"role":"user", "content":huge}]}),
    );
    let usage = crate::Usage::new(298_468, 1_000, 0, 100_000, 20_000);
    let mut anchor = ContextRequestAnchor::new(
        binding,
        RouteId::new("test-route").expect("route"),
        ModelId::new("requested-model").expect("model"),
        usage,
        old,
    )
    .expect("anchor");
    let encoded = serde_json::to_value(&anchor).expect("anchor wire");
    assert_eq!(
        serde_json::from_value::<ContextRequestAnchor>(encoded.clone()).expect("anchor restore"),
        anchor
    );
    let mut zero_usage = encoded;
    zero_usage["usage"] =
        serde_json::to_value(crate::Usage::new(0, 0, 0, 0, 0)).expect("zero usage");
    assert!(serde_json::from_value::<ContextRequestAnchor>(zero_usage).is_err());
    let current = wire_input(
        &serde_json::json!({"instructions":"clock=2", "input":[{"role":"user", "content":huge}, {"role":"assistant", "content":"a".repeat(4000)}, {"type":"function_call_output", "output":"tool-result"}]}),
    );
    let authority_identity = anchor.identity().expect("authority identity");
    anchor.observe_response_items(&current);
    assert_eq!(
        anchor.identity().expect("cached identity"),
        authority_identity
    );
    let new_system = json_bytes(&serde_json::json!("clock=2"));
    let result =
        json_bytes(&serde_json::json!({"type":"function_call_output", "output":"tool-result"}));
    assert_eq!(
        anchor.occupied_tokens(&current),
        298_468 + 1_000 + new_system + result + 32
    );
    let status = item_status(&current, Some(&anchor), binding, false);
    assert!(matches!(status.kind(), ContextStatusKind::Fits { .. }));
    assert_eq!(
        status.occupancy_tokens(),
        Some(anchor.occupied_tokens(&current))
    );
    assert!(!status.uses_unanchored_estimate());
    // A later zero-usage response cannot spend the prior output credit.
    let later = wire_input(
        &serde_json::json!({"instructions":"clock=2", "input":[{"role":"user", "content":huge}, {"role":"assistant", "content":"changed-output"}, {"type":"function_call_output", "output":"tool-result"}, {"role":"assistant", "content":"later-response"}]}),
    );
    assert!(anchor.occupied_tokens(&later) > 299_468 + new_system + result + 32);
}

#[test]
fn item_delta_removal_gives_no_credit_and_duplicate_items_are_new() {
    let binding = authority(b"binding");
    let old = wire_input(&serde_json::json!({"system":["one","two"],"messages":[]}));
    let anchor = ContextRequestAnchor::new(
        binding,
        RouteId::new("test-route").expect("route"),
        ModelId::new("requested-model").expect("model"),
        crate::Usage::new(100, 0, 0, 0, 0),
        old,
    )
    .expect("anchor");
    assert_eq!(
        anchor.occupied_tokens(&wire_input(
            &serde_json::json!({"system":["one"],"messages":[]})
        )),
        100
    );
    assert_eq!(
        anchor.occupied_tokens(&wire_input(
            &serde_json::json!({"system":["one","one"],"messages":[]})
        )),
        100 + 5 + 16
    );
}

#[test]
fn unanchored_resume_import_and_model_switch_delegate_a_plausible_big_request() {
    let input = wire_input(
        &serde_json::json!({"input":[{"role":"user","content":"x".repeat(850_715 * 4)}]}),
    );
    let old_binding = authority(b"old-binding");
    let binding = authority(b"new-binding");
    let anchor = ContextRequestAnchor::new(
        old_binding,
        RouteId::new("old-route").expect("route"),
        ModelId::new("old-model").expect("model"),
        crate::Usage::new(298_468, 1000, 0, 0, 0),
        input.clone(),
    )
    .expect("anchor");
    let mut foreign_model = anchor.clone();
    foreign_model.binding = binding;
    foreign_model.route = RouteId::new("test-route").expect("route");
    let mut foreign_route = foreign_model.clone();
    foreign_route.model = ModelId::new("requested-model").expect("model");
    foreign_route.route = RouteId::new("foreign-route").expect("route");
    for anchor in [
        None,
        Some(&anchor),
        Some(&foreign_model),
        Some(&foreign_route),
    ] {
        let status = item_status(&input, anchor, binding, false);
        assert!(matches!(status.kind(), ContextStatusKind::Fits { .. }));
        assert!(status.uses_unanchored_estimate());
        assert!(matches!(
            status.admit_with_requirement(
                &request(Some(128_000)),
                ContextAdmissionRequirement::ReleaseVerifiedRequired
            ),
            Err(ContextAdmissionError::Unavailable(_))
        ));
    }
    let too_large = wire_input(
        &serde_json::json!({"input":[{"role":"user","content":"x".repeat(900_000 * 4)}]}),
    );
    assert!(matches!(
        item_status(&too_large, None, binding, false).kind(),
        ContextStatusKind::ProtectedOverflow(_)
    ));
}

#[test]
fn fold_input_requires_a_bound_even_when_a_huge_tool_result_estimate_fits() {
    let binding = authority(b"binding");
    let huge = wire_input(
        &serde_json::json!({"input":[{"type":"function_call_output","output":"x".repeat(850_715 * 4)}]}),
    );
    assert!(matches!(
        item_status(&huge, None, binding, false).kind(),
        ContextStatusKind::Fits { .. }
    ));
    assert!(matches!(
        item_status(&huge, None, binding, true).kind(),
        ContextStatusKind::ProtectedOverflow(_)
    ));
    let elided = wire_input(
        &serde_json::json!({"input":[{"type":"function_call_output","output":"tool result elided"}]}),
    );
    assert!(matches!(
        item_status(&elided, None, binding, true).kind(),
        ContextStatusKind::Fits { .. }
    ));
}

#[test]
fn requested_output_is_reserved_once_only_on_shared_windows() {
    let binding = authority(b"binding");
    let input =
        wire_input(&serde_json::json!({"input":[{"role":"user","content":"q".repeat(900_000)}]}));
    let anchor = ContextRequestAnchor::new(
        binding,
        RouteId::new("test-route").expect("route"),
        ModelId::new("requested-model").expect("model"),
        crate::Usage::new(850_000, 10_000, 0, 0, 0),
        input.clone(),
    )
    .expect("anchor");
    assert!(matches!(
        item_status(&input, Some(&anchor), binding, false).kind(),
        ContextStatusKind::Fits {
            headroom: ContextHeadroom::Shared(QualifiedTokens::GuaranteedMinimum(2000))
        }
    ));
    assert!(
        evaluate_rule(
            CapacityRule::ProviderReservedInput {
                reported_context_window: Some(1_000_000),
                maximum_projected_input: 1_000_000,
                auto_compact_input: None
            },
            CountBound::Exact(900_000),
            Some(128_000)
        )
        .is_ok()
    );
}

#[test]
fn image_input_uses_the_dialect_bound_not_base64_and_digests_exact_wire_items() {
    let small = wire_input(
        &serde_json::json!({"messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","data":"abcd"}}]}]}),
    );
    let huge = wire_input(
        &serde_json::json!({"messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","data":"a".repeat(4_000_000)}}]}]}),
    );
    assert_eq!(small.upper_bound(), huge.upper_bound());
    assert_eq!(small.estimate_tokens(), huge.estimate_tokens());
    let small_body = serde_json::to_vec(&serde_json::json!({"messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","data":"abcd"}}]}]})).expect("small body");
    let huge_body = serde_json::to_vec(&serde_json::json!({"messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","data":"a".repeat(4_000_000)}}]}]})).expect("huge body");
    let small_parts = ContextInputParts::measure_wire(&small_body)
        .expect("small parts")
        .with_image_tokens(small.image_tokens());
    let huge_parts = ContextInputParts::measure_wire(&huge_body)
        .expect("huge parts")
        .with_image_tokens(huge.image_tokens());
    assert_eq!(
        small_parts.occupancy_tokens(),
        huge_parts.occupancy_tokens()
    );
    assert_eq!(small_parts.message_bytes(), huge_parts.message_bytes());
    assert!(huge_parts.image_transport_bytes() > 3_999_000);
    assert_ne!(small.items[0].digest, huge.items[0].digest);
    assert!(small.upper_bound() >= CONSERVATIVE_IMAGE_MAX_TOKENS);
    let first =
        ContextWireInput::measure_wire(br#"{"input":[{"role":"user","content":"q"}]}"#, None)
            .expect("wire");
    let reordered =
        ContextWireInput::measure_wire(br#"{"input":[{"content":"q","role":"user"}]}"#, None)
            .expect("wire");
    assert_ne!(first.items[0].digest, reordered.items[0].digest);
}

fn prepared_status(
    descriptor: &ResolvedModelDescriptor,
    request: &ModelRequest,
    account: Digest,
    observation: Option<CountObservation>,
    now: Timestamp,
) -> ContextStatus {
    let (plan, projection) = prepared(descriptor, request, account, observation);
    descriptor
        .context_status(&plan, &projection, now)
        .expect("context status")
}

struct InspectingModel {
    descriptor: ResolvedModelDescriptor,
    authority: RuntimeModelAuthority,
    observation: Option<CountObservation>,
    input_parts: Option<ContextInputParts>,
    prepares: Arc<AtomicUsize>,
    starts: Arc<AtomicUsize>,
}

impl InspectingModel {
    fn new(descriptor: ResolvedModelDescriptor, observation: Option<CountObservation>) -> Self {
        let authority = RuntimeModelAuthority::new(
            &descriptor,
            authority(b"inspection-account"),
            credential_authority(),
            1,
            timestamp(1),
            Some(timestamp(100)),
        )
        .expect("inspection authority");
        Self {
            descriptor,
            authority,
            observation,
            input_parts: None,
            prepares: Arc::new(AtomicUsize::new(0)),
            starts: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn prepare(
        self: &Arc<Self>,
        request: ModelRequest,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        let plan = self
            .descriptor
            .validate_request(request, ValidationMode::Strict)
            .expect("inspection plan");
        let context = ModelPreparationContext::new(self.authority.clone(), timestamp(2));
        let model: Arc<dyn LanguageModel> = self.clone();
        model.prepare_call(plan, context)
    }
}

impl LanguageModel for InspectingModel {
    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        self.prepares.fetch_add(1, Ordering::AcqRel);
        self.descriptor.validate_plan_binding(&plan)?;
        context
            .authority()
            .validate(&self.descriptor, context.prepared_at())?;
        let wire =
            serde_json::to_vec(&plan.request().model_input_identity()).expect("inspection wire");
        let wire_digest = Digest::sha256(DigestDomain::Blob, 1, &[b"inspection-wire-v1", &wire])
            .expect("inspection wire digest");
        let token_projection = PreparedTokenProjection::new(
            wire_digest,
            u64::try_from(wire.len()).expect("inspection wire size"),
            self.observation.clone(),
        )?
        .with_input_parts(self.input_parts);
        let output_limit = plan.request().limits().max_output_tokens().map(|tokens| {
            PreparedOutputLimit::enforced_wire_maximum(
                tokens,
                "max_output_tokens",
                wire_digest,
                "inspection-lowering-1",
            )
            .expect("inspection output limit")
        });
        let contract = self.descriptor.context_contract();
        let projection = PreparedModelProjection::new(
            &self.descriptor,
            &plan,
            &context,
            contract.map_or_else(
                || self.descriptor.protocol().clone(),
                |value| value.protocol_family().clone(),
            ),
            "inspection-lowering-1",
            contract.map_or(
                "inspection-projection-1",
                ContextContract::projection_revision,
            ),
            contract.map_or(
                "inspection-accounting-1",
                ContextContract::accounting_revision,
            ),
            wire_digest,
            u64::try_from(wire.len()).expect("inspection wire size"),
            token_projection,
            PreparedCacheUse::None,
            output_limit,
        )?;
        let record =
            PreparedCallRecord::for_prepared(&self.descriptor, &plan, projection, &context)?;
        let starts = Arc::clone(&self.starts);
        Ok(PreparedModelCall::new(record, move |_| {
            starts.fetch_add(1, Ordering::AcqRel);
            let stream: ModelStream = Box::pin(futures_util::stream::empty());
            Box::pin(async move { Ok(stream) })
        }))
    }
}

fn eligible_overflow_request() -> ModelRequest {
    let input = ConversationView::from_shared(
        CanonicalHistory::new(vec![
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text_item(20, Actor::User, "older")],
            )
            .expect("older intent"),
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![text_item(21, Actor::Assistant, "older answer")],
            )
            .expect("older answer"),
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text_item(22, Actor::User, "latest")],
            )
            .expect("latest intent"),
        ])
        .expect("eligible history"),
        WorkingProjection::default(),
    );
    request_with_input(input, Some(20))
}

#[test]
fn ordinary_prepare_exposes_only_the_exact_admitted_status_identity_before_start() {
    let descriptor = descriptor(
        CapacityRule::SharedWindow {
            total: SafeLimit::Exact(100),
        },
        measurement_certificate(CountCertificateKind::ExactTokenizer),
    );
    let model = Arc::new(InspectingModel::new(descriptor, Some(observation(60))));
    let prepared = model.prepare(request(Some(20))).expect("fits");
    assert_eq!(model.prepares.load(Ordering::Acquire), 1);
    assert_eq!(model.starts.load(Ordering::Acquire), 0);
    assert_eq!(
        prepared.record().context_status_identity().domain(),
        DigestDomain::ContextAdmission
    );
    assert_eq!(prepared.record().context_status_identity().version(), 3);
    assert_eq!(prepared.record().context_admission_identity().version(), 4);
    assert_eq!(prepared.record().identity().version(), 9);
    let encoded = serde_json::to_vec(prepared.record()).expect("prepared record encode");
    assert!(encoded.len() <= MAX_PREPARED_CALL_RECORD_BYTES);
    assert!(
        !encoded
            .windows(b"Fits".len())
            .any(|window| window == b"Fits")
    );
    let decoded: PreparedCallRecord =
        serde_json::from_slice(&encoded).expect("prepared record decode");
    assert_eq!(decoded, *prepared.record());
    drop(prepared);
    assert_eq!(model.starts.load(Ordering::Acquire), 0);
}

#[test]
fn prepared_projection_binds_required_provider_configuration_ids() {
    let descriptor = descriptor(
        CapacityRule::SharedWindow {
            total: SafeLimit::Exact(100),
        },
        measurement_certificate(CountCertificateKind::ExactTokenizer),
    );
    let request = request(Some(20));
    let (plan, projection) = prepared(
        &descriptor,
        &request,
        authority(b"projection-account"),
        Some(observation(60)),
    );
    assert_eq!(
        projection.selected_provider_configuration_id(),
        request.selected_provider_configuration_id()
    );
    assert_eq!(
        projection.effective_provider_configuration_id(),
        request.effective_provider_configuration_id()
    );
    projection
        .validate_for_plan(&descriptor, &plan)
        .expect("matching plan");

    let encoded = serde_json::to_value(&projection).expect("projection encode");
    let decoded: PreparedModelProjection =
        serde_json::from_value(encoded.clone()).expect("projection decode");
    assert_eq!(decoded, projection);

    let mut missing = encoded.clone();
    missing
        .as_object_mut()
        .expect("projection object")
        .remove("effective_provider_configuration");
    assert!(serde_json::from_value::<PreparedModelProjection>(missing).is_err());

    let mut swapped = encoded;
    let object = swapped.as_object_mut().expect("projection object");
    let selected = object["selected_provider_configuration"].clone();
    let effective = object["effective_provider_configuration"].clone();
    object.insert("selected_provider_configuration".into(), effective);
    object.insert("effective_provider_configuration".into(), selected);
    assert!(serde_json::from_value::<PreparedModelProjection>(swapped).is_err());
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one preparation matrix proves complete status/source identity and zero-start behavior for every closed denial kind"
)]
fn ordinary_prepare_retains_complete_exact_status_for_every_denial_before_start() {
    let mut unavailable_features = BTreeMap::new();
    unavailable_features.insert(
        Capability::Streaming,
        FeatureSupport::new(
            Support::Native,
            CapabilityConstraints::default(),
            FactProvenance::new(FactSource::TestFixture, timestamp(1), None),
        ),
    );
    let unavailable_model = Arc::new(InspectingModel::new(
        ResolvedModelDescriptor::new(
            &StandardIntegrationAuthority::release_owned(),
            ProtocolId::new("private-dialect").expect("protocol"),
            RouteId::new("private-route").expect("route"),
            ModelId::new("private-model").expect("model"),
            None,
            None,
            ModelAdapterProfileId::new("private-adapter").expect("adapter"),
            CapabilitySet::new(unavailable_features, BTreeMap::new()),
        ),
        None,
    ));
    let unavailable_request = request(Some(20));
    let unavailable = unavailable_model
        .prepare(unavailable_request.clone())
        .expect_err("missing contract must deny");
    let unavailable_status = unavailable
        .context_status()
        .expect("complete unavailable status");
    assert_eq!(
        unavailable_status.kind(),
        &ContextStatusKind::Unavailable(ContextUnavailable::ContractUnavailable)
    );
    assert_eq!(
        unavailable.context_admission_error(),
        Some(&ContextAdmissionError::Unavailable(
            ContextUnavailable::ContractUnavailable
        ))
    );
    assert_eq!(
        unavailable_status.model_input_identity(),
        unavailable_request.model_input_identity()
    );
    assert!(unavailable_status.scope().is_none());
    assert_eq!(unavailable_model.prepares.load(Ordering::Acquire), 1);
    assert_eq!(unavailable_model.starts.load(Ordering::Acquire), 0);
    let debug = format!("{unavailable:?}");
    assert!(debug.contains("<complete context status retained>"));
    assert!(!debug.contains("private-route"));
    assert!(!debug.contains("private-model"));
    assert_eq!(
        ModelError::from(unavailable.clone()).kind(),
        ModelErrorKind::InvalidRequest
    );

    let overflow_descriptor = descriptor(
        CapacityRule::Separate {
            input: SafeLimit::Exact(50),
            output: SafeLimit::Exact(10),
        },
        measurement_certificate(CountCertificateKind::ExactTokenizer),
    );
    for (request, expected_protected) in [
        (eligible_overflow_request(), false),
        (request(Some(20)), true),
    ] {
        let model = Arc::new(InspectingModel::new(
            overflow_descriptor.clone(),
            Some(observation(60)),
        ));
        let error = model
            .prepare(request.clone())
            .expect_err("overflow must deny");
        let status = error.context_status().expect("complete overflow status");
        assert_eq!(
            status.model_input_identity(),
            request.model_input_identity()
        );
        assert_eq!(
            status.descriptor_binding(),
            overflow_descriptor
                .binding_digest()
                .expect("descriptor binding")
        );
        let scope = status.scope().expect("overflow scope");
        assert_eq!(scope.protocol(), overflow_descriptor.protocol());
        assert_eq!(scope.route(), overflow_descriptor.route());
        assert_eq!(
            scope.requested_model(),
            overflow_descriptor.requested_model()
        );
        assert_eq!(scope.lowering_revision(), "inspection-lowering-1");
        assert_eq!(scope.model_input_identity(), request.model_input_identity());
        assert_eq!(
            scope.prepared_token_projection(),
            status
                .measurement()
                .expect("measurement")
                .scope()
                .prepared_token_projection()
        );
        assert_eq!(
            status
                .measurement()
                .expect("measurement")
                .bound()
                .upper_bound(),
            60
        );
        assert_eq!(status.reservation().expect("reservation").tokens(), 20);
        let planner = status.planner_disposition().expect("planner disposition");
        match (
            expected_protected,
            status.kind(),
            error.context_admission_error(),
        ) {
            (
                false,
                ContextStatusKind::WouldOverflow(status_overflow),
                Some(ContextAdmissionError::WouldOverflow(error_overflow)),
            ) => {
                assert_eq!(status_overflow, error_overflow);
                assert_eq!(
                    planner.kind(),
                    ContextPlannerDispositionKind::EligibleWholeGroups
                );
            }
            (
                true,
                ContextStatusKind::ProtectedOverflow(status_overflow),
                Some(ContextAdmissionError::ProtectedOverflow(error_overflow)),
            ) => {
                assert_eq!(status_overflow, error_overflow);
                assert_eq!(planner.kind(), ContextPlannerDispositionKind::ProtectedOnly);
            }
            other => panic!("mismatched retained status and denial: {other:?}"),
        }
        assert_eq!(model.prepares.load(Ordering::Acquire), 1);
        assert_eq!(model.starts.load(Ordering::Acquire), 0);
        assert_eq!(ModelError::from(error).kind(), ModelErrorKind::ContextLimit);
    }
}

#[test]
fn exact_scope_and_attempt_metadata_exclusion_are_preserved() {
    let descriptor = descriptor(
        CapacityRule::SharedWindow {
            total: SafeLimit::Exact(100),
        },
        measurement_certificate(CountCertificateKind::ExactTokenizer),
    );
    let request = request(Some(20));
    let status = prepared_status(
        &descriptor,
        &request,
        authority(b"account-a"),
        Some(observation(70)),
        timestamp(2),
    );
    let scope = status.scope().expect("scope");
    assert_eq!(scope.protocol_family().as_str(), "responses-family");
    assert_eq!(scope.protocol().as_str(), "test-dialect");
    assert_eq!(scope.route().as_str(), "test-route");
    assert_eq!(scope.requested_model().as_str(), "requested-model");
    assert_eq!(scope.effective_model().as_str(), "effective-model");
    assert_eq!(scope.adapter().as_str(), "test-adapter");
    assert_eq!(scope.model_input_identity(), request.model_input_identity());
    assert!(matches!(
        status.kind(),
        ContextStatusKind::Fits {
            headroom: ContextHeadroom::Shared(QualifiedTokens::Exact(10))
        }
    ));
    assert!(status.planner_disposition().is_none());

    let metadata_only = request
        .for_round(RequestMetadata::for_turn(
            request.metadata().run_id(),
            request.metadata().turn_id().expect("turn-rooted fixture"),
            AttemptId::try_from(Uuid::from_u128(4)).expect("attempt ID"),
            timestamp(3),
        ))
        .expect("metadata-only request");
    assert_ne!(request.identity(), metadata_only.identity());
    assert_eq!(
        request.model_input_identity(),
        metadata_only.model_input_identity()
    );
    let metadata_status = prepared_status(
        &descriptor,
        &metadata_only,
        authority(b"account-a"),
        Some(observation(70)),
        timestamp(2),
    );
    assert_eq!(status.identity(), metadata_status.identity());
}

/// The fences and the gauge read part bytes/4, plus the provider input
/// and suffix when anchored; records read the unanchored estimate (the
/// bound without parts); the anchor never enters the status identity.
#[test]
fn status_occupancy_is_the_one_anchored_measure() {
    let descriptor = descriptor(
        CapacityRule::SharedWindow {
            total: SafeLimit::Exact(100),
        },
        measurement_certificate(CountCertificateKind::ExactTokenizer),
    );
    let status = prepared_status(
        &descriptor,
        &request(Some(20)),
        authority(b"account-a"),
        Some(observation(70)),
        timestamp(2),
    );
    assert_eq!(status.occupancy_tokens(), Some(70), "bound fallback");
    // Without parts the bound is on another scale: an anchor never
    // applies to it, and the fences see no estimate at all.
    let unparted = status
        .clone()
        .with_context_anchor(ContextAnchor::new(5, 10));
    assert_eq!(unparted.occupancy_estimate_tokens(), None);
    assert_eq!(unparted.occupancy_tokens(), Some(70));
    assert_eq!(unparted.unanchored_occupancy_tokens(), Some(70));
    let body = vec![b'x'; 4_000];
    let body = format!(
        r#"{{"input":"{}"}}"#,
        String::from_utf8(body).expect("ascii")
    );
    let parts = ContextInputParts::measure_wire(body.as_bytes()).expect("object body");
    let measured = status.clone().with_input_parts(Some(parts));
    let estimate = parts.occupancy_tokens();
    assert_eq!(measured.occupancy_estimate_tokens(), Some(estimate));
    assert_eq!(measured.occupancy_tokens(), Some(estimate));

    let anchor = ContextAnchor::new(300, estimate - 250);
    let anchored = measured.clone().with_context_anchor(anchor);
    assert_eq!(anchored.occupancy_tokens(), Some(300 + 250));
    assert_eq!(
        anchored.occupancy_tokens(),
        Some(context_occupancy_tokens(estimate, anchor))
    );
    assert_eq!(anchored.occupancy_estimate_tokens(), Some(estimate));
    assert_eq!(anchored.unanchored_occupancy_tokens(), Some(estimate));
    assert_eq!(anchored.identity(), status.identity());
}

/// An admission denial carries the byte parts its request was measured
/// with, so a recovery reading of an overflowing request stays on the
/// anchor's scale instead of falling back to the measurement bound.
#[test]
fn an_admission_denial_carries_the_measured_input_parts() {
    let overflow_descriptor = descriptor(
        CapacityRule::Separate {
            input: SafeLimit::Exact(50),
            output: SafeLimit::Exact(10),
        },
        measurement_certificate(CountCertificateKind::ExactTokenizer),
    );
    let body = format!(r#"{{"input":"{}"}}"#, "x".repeat(400));
    let parts = ContextInputParts::measure_wire(body.as_bytes()).expect("object body");
    let mut model = InspectingModel::new(overflow_descriptor, Some(observation(60)));
    model.input_parts = Some(parts);
    let error = Arc::new(model)
        .prepare(request(Some(20)))
        .expect_err("overflow must deny");
    let status = error.context_status().expect("complete overflow status");
    assert_eq!(status.input_parts(), Some(parts));
    assert_eq!(
        status.occupancy_estimate_tokens(),
        Some(parts.occupancy_tokens())
    );
    let anchor = ContextAnchor::new(7, parts.occupancy_tokens() - 3);
    assert_eq!(
        status
            .clone()
            .with_context_anchor(anchor)
            .occupancy_tokens(),
        Some(10)
    );
}

#[test]
fn release_verified_upper_bound_and_lower_capacity_never_claim_exact_headroom() {
    let corpus =
        Digest::sha256(DigestDomain::TokenMeasurement, 99, &[b"proof-corpus"]).expect("corpus");
    let descriptor = descriptor(
        CapacityRule::SharedWindowWithOutputCap {
            total: SafeLimit::GuaranteedLowerBound(120),
            output: SafeLimit::Exact(40),
        },
        measurement_certificate(CountCertificateKind::ReleaseVerifiedUpperBound { corpus }),
    );
    let request = request(Some(30));
    let status = prepared_status(
        &descriptor,
        &request,
        authority(b"account-a"),
        Some(observation(80)),
        timestamp(2),
    );
    let measurement = status.measurement().expect("measurement");
    assert!(matches!(
        measurement.bound(),
        CountBound::ReleaseVerifiedUpperBound {
            tokens: 80,
            corpus: found,
            ..
        } if found == corpus
    ));
    assert!(matches!(
        status.kind(),
        ContextStatusKind::Fits {
            headroom: ContextHeadroom::SharedWithOutputCap {
                shared: QualifiedTokens::GuaranteedMinimum(10),
                output: QualifiedTokens::Exact(10),
            }
        }
    ));
}

#[test]
fn missing_unknown_drifted_and_expired_evidence_fail_closed() {
    let request = request(Some(20));
    let provenance = FactProvenance::new(FactSource::TestFixture, timestamp(1), None);
    let mut features = BTreeMap::new();
    features.insert(
        Capability::Streaming,
        FeatureSupport::new(
            Support::Native,
            CapabilityConstraints::default(),
            provenance,
        ),
    );
    let plain = ResolvedModelDescriptor::new(
        &StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("test-dialect").expect("protocol"),
        RouteId::new("test-route").expect("route"),
        ModelId::new("requested-model").expect("model"),
        None,
        None,
        ModelAdapterProfileId::new("test-adapter").expect("adapter"),
        CapabilitySet::new(features, BTreeMap::new()),
    );
    let missing = prepared_status(
        &plain,
        &request,
        authority(b"account-a"),
        None,
        timestamp(2),
    );
    assert_eq!(
        missing.kind(),
        &ContextStatusKind::Unavailable(ContextUnavailable::ContractUnavailable)
    );

    let descriptor = descriptor(
        CapacityRule::SharedWindow {
            total: SafeLimit::Exact(100),
        },
        measurement_certificate(CountCertificateKind::ExactTokenizer),
    );
    for (candidate, expected) in [
        (
            CountObservation::new("unknown-counter", "cert-1", 50).expect("candidate"),
            ContextUnavailable::UnknownMeasurementCertificate,
        ),
        (
            CountObservation::new("fixture-counter", "wrong-revision", 50).expect("candidate"),
            ContextUnavailable::MeasurementCertificateRevisionDrift,
        ),
    ] {
        let status = prepared_status(
            &descriptor,
            &request,
            authority(b"account-a"),
            Some(candidate),
            timestamp(2),
        );
        assert_eq!(status.kind(), &ContextStatusKind::Unavailable(expected));
    }

    let expired = prepared_status(
        &descriptor,
        &request,
        authority(b"account-a"),
        Some(observation(50)),
        timestamp(100),
    );
    assert_eq!(
        expired.kind(),
        &ContextStatusKind::Unavailable(ContextUnavailable::EvidenceExpired {
            evidence: "capacity"
        })
    );
}

#[test]
fn provider_reserved_input_requires_a_positive_ceiling_within_the_reported_window() {
    let freshness = EvidenceFreshness::new(timestamp(1), Some(timestamp(10))).expect("freshness");
    let certificate = |rule| {
        CapacityCertificate::new(
            &StandardIntegrationAuthority::release_owned(),
            "capacity",
            "1",
            ContextEvidenceSource::BuiltInCatalog,
            rule,
            None,
            freshness,
        )
    };
    assert!(matches!(
        certificate(CapacityRule::ProviderReservedInput {
            reported_context_window: Some(1),
            maximum_projected_input: 0,
            auto_compact_input: None,
        }),
        Err(ContextContractError::ZeroLimit { .. })
    ));
    assert_eq!(
        certificate(CapacityRule::ProviderReservedInput {
            reported_context_window: Some(1),
            maximum_projected_input: 2,
            auto_compact_input: None,
        }),
        Err(ContextContractError::InvalidCapacityRelationship)
    );
}

#[test]
fn invalid_or_product_shaped_evidence_never_reaches_descriptor_authority() {
    let freshness = EvidenceFreshness::new(timestamp(1), Some(timestamp(10))).expect("freshness");
    assert!(matches!(
        CapacityCertificate::new(
            &StandardIntegrationAuthority::release_owned(),
            "capacity",
            "1",
            ContextEvidenceSource::BuiltInCatalog,
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(0)
            },
            None,
            freshness,
        ),
        Err(ContextContractError::ZeroLimit { .. })
    ));
    assert_eq!(
        CapacityCertificate::new(
            &StandardIntegrationAuthority::release_owned(),
            "capacity",
            "1",
            ContextEvidenceSource::AuthenticatedDiscovery,
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(1)
            },
            None,
            freshness,
        ),
        Err(ContextContractError::UnscopedDiscovery)
    );
    assert_eq!(
        CapacityCertificate::new(
            &StandardIntegrationAuthority::release_owned(),
            "capacity",
            "1",
            ContextEvidenceSource::BuiltInCatalog,
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(1)
            },
            Some(authority(b"foreign-authority")),
            freshness,
        ),
        Err(ContextContractError::UnexpectedAuthorityBinding)
    );
    assert_eq!(
        CapacityCertificate::new(
            &StandardIntegrationAuthority::release_owned(),
            "capacity",
            "1",
            ContextEvidenceSource::BuiltInCatalog,
            CapacityRule::ProviderReservedInput {
                reported_context_window: Some(1),
                maximum_projected_input: 1,
                auto_compact_input: None,
            },
            None,
            freshness,
        ),
        Err(ContextContractError::EvidenceClassMismatch)
    );
    let corpus = Digest::sha256(DigestDomain::TokenMeasurement, 99, &[b"corpus"]).expect("corpus");
    assert_eq!(
        MeasurementCertificate::standard(
            &StandardIntegrationAuthority::release_owned(),
            "counter",
            "1",
            "bound",
            "1",
            ContextEvidenceSource::BuiltInCatalog,
            CountCertificateKind::ReleaseVerifiedUpperBound { corpus },
            None,
            freshness,
        ),
        Err(ContextContractError::UpperBoundNotReleaseVerified)
    );
    assert!(
        serde_json::from_str::<ContextEvidenceSource>(r#""UserOverride""#).is_err(),
        "product/user override is not an evidence authority variant"
    );
}

#[test]
fn discovery_is_authority_scoped_and_explicit_reservation_is_mandatory() {
    let account_a = authority(b"account-a");
    let account_b = authority(b"account-b");
    let discovery_capacity = CapacityCertificate::new(
        &StandardIntegrationAuthority::release_owned(),
        "discovered-capacity",
        "1",
        ContextEvidenceSource::AuthenticatedDiscovery,
        CapacityRule::Separate {
            input: SafeLimit::Exact(80),
            output: SafeLimit::Exact(20),
        },
        Some(authority_class(account_a)),
        EvidenceFreshness::new(timestamp(1), Some(timestamp(10))).expect("freshness"),
    )
    .expect("capacity");
    let contract = ContextContract::new(
        ProtocolId::new("responses-family").expect("family"),
        "accounting-1",
        "projection-1",
        "reservation-1",
        discovery_capacity,
        vec![measurement_certificate(
            CountCertificateKind::ExactProviderCounter,
        )],
    )
    .expect("contract");
    let descriptor = descriptor(
        CapacityRule::SharedWindow {
            total: SafeLimit::Exact(1),
        },
        measurement_certificate(CountCertificateKind::ExactProviderCounter),
    )
    .with_context_contract(contract);
    let reserved_request = request(Some(10));
    let foreign = prepared_status(
        &descriptor,
        &reserved_request,
        account_b,
        Some(observation(40)),
        timestamp(2),
    );
    assert_eq!(
        foreign.kind(),
        &ContextStatusKind::Unavailable(ContextUnavailable::ScopeDrift {
            field: "capacity authority binding"
        })
    );

    let without_reservation = prepared_status(
        &descriptor,
        &request(None),
        account_a,
        Some(observation(40)),
        timestamp(2),
    );
    assert_eq!(
        without_reservation.kind(),
        &ContextStatusKind::Unavailable(ContextUnavailable::ExplicitOutputReservationUnavailable)
    );
}

#[test]
fn overflow_uses_only_the_internally_derived_closed_history_plan() {
    let descriptor = descriptor(
        CapacityRule::Separate {
            input: SafeLimit::Exact(50),
            output: SafeLimit::Exact(10),
        },
        measurement_certificate(CountCertificateKind::ExactTokenizer),
    );
    let protected_request = request(Some(20));
    let protected = prepared_status(
        &descriptor,
        &protected_request,
        authority(b"account-a"),
        Some(observation(60)),
        timestamp(2),
    );
    let ContextStatusKind::ProtectedOverflow(protected_overflow) = protected.kind() else {
        panic!("expected protected overflow")
    };
    assert_eq!(protected_overflow.input().expect("input").required(), 60);
    assert_eq!(protected_overflow.output().expect("output").required(), 20);
    assert_eq!(
        protected.admit(&protected_request),
        Err(ContextAdmissionError::ProtectedOverflow(
            *protected_overflow
        ))
    );
    let protected_plan = protected
        .planner_disposition()
        .expect("protected planner disposition");
    assert_eq!(
        protected_plan.kind(),
        ContextPlannerDispositionKind::ProtectedOnly
    );
    assert_eq!(protected_plan.selected_group_count(), 0);

    let input = ConversationView::from_shared(
        CanonicalHistory::new(vec![
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text_item(10, Actor::User, "older")],
            )
            .expect("older intent"),
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![text_item(11, Actor::Assistant, "older answer")],
            )
            .expect("older answer"),
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text_item(12, Actor::User, "latest")],
            )
            .expect("latest intent"),
        ])
        .expect("history"),
        WorkingProjection::default(),
    );
    let eligible_request = request_with_input(input, Some(20));
    let eligible = prepared_status(
        &descriptor,
        &eligible_request,
        authority(b"account-a"),
        Some(observation(60)),
        timestamp(2),
    );
    let ContextStatusKind::WouldOverflow(overflow) = eligible.kind() else {
        panic!("expected whole-group overflow")
    };
    assert_eq!(overflow.input().expect("input").required(), 60);
    assert_eq!(overflow.output().expect("output").required(), 20);
    let planner = eligible
        .planner_disposition()
        .expect("eligible planner disposition");
    assert_eq!(
        planner.kind(),
        ContextPlannerDispositionKind::EligibleWholeGroups
    );
    assert_eq!(planner.selected_group_count(), 1);
    assert_eq!(planner.selected_segment_count(), 2);
    assert_eq!(
        eligible.admit(&eligible_request),
        Err(ContextAdmissionError::WouldOverflow(*overflow))
    );
}

#[test]
fn context_status_rejects_expired_evidence_and_foreign_descriptor() {
    let certificate = measurement_certificate(CountCertificateKind::ExactTokenizer);
    let descriptor = descriptor(
        CapacityRule::SharedWindow {
            total: SafeLimit::Exact(100),
        },
        certificate.clone(),
    );
    let request = request(Some(20));
    let (plan, projection) = prepared(
        &descriptor,
        &request,
        authority(b"account-a"),
        Some(observation(60)),
    );
    let admission = descriptor
        .context_status(&plan, &projection, timestamp(2))
        .expect("status")
        .admit(plan.request())
        .expect("admission");
    assert_eq!(admission.status().identity().version(), 3);
    assert_eq!(admission.identity().version(), 4);
    assert_eq!(
        descriptor
            .context_status(&plan, &projection, timestamp(100))
            .expect("expired status")
            .admit(plan.request()),
        Err(ContextAdmissionError::Unavailable(
            ContextUnavailable::EvidenceExpired {
                evidence: "capacity"
            }
        ))
    );

    let foreign = ResolvedModelDescriptor::new(
        &StandardIntegrationAuthority::release_owned(),
        descriptor.protocol().clone(),
        RouteId::new("foreign-route").expect("route"),
        descriptor.requested_model().clone(),
        descriptor.reported_model().cloned(),
        descriptor.model_family().cloned(),
        descriptor.profile().clone(),
        descriptor.capabilities().clone(),
    )
    .with_context_contract(contract(
        CapacityRule::SharedWindow {
            total: SafeLimit::Exact(100),
        },
        certificate,
    ));
    assert_eq!(
        foreign
            .context_status(&plan, &projection, timestamp(2))
            .expect("foreign status")
            .admit(plan.request()),
        Err(ContextAdmissionError::Unavailable(
            ContextUnavailable::ScopeDrift {
                field: "prepared projection"
            }
        ))
    );
}

#[test]
fn contract_deserialization_recomputes_identity() {
    let contract = contract(
        CapacityRule::SharedWindow {
            total: SafeLimit::Exact(100),
        },
        measurement_certificate(CountCertificateKind::ExactTokenizer),
    );
    let encoded = serde_json::to_string(&contract).expect("serialize");
    let decoded: ContextContract = serde_json::from_str(&encoded).expect("deserialize");
    assert_eq!(decoded, contract);

    let mut forged = serde_json::to_value(contract).expect("value");
    forged["accounting_revision"] = serde_json::json!("forged-revision");
    assert!(serde_json::from_value::<ContextContract>(forged).is_err());
}

#[test]
fn descriptor_request_validation_still_uses_exact_binding() {
    let descriptor = descriptor(
        CapacityRule::SharedWindow {
            total: SafeLimit::Exact(100),
        },
        measurement_certificate(CountCertificateKind::ExactTokenizer),
    );
    let request = request(Some(20));
    let plan = descriptor
        .validate_request(request.clone(), ValidationMode::Strict)
        .expect("plan");
    descriptor
        .validate_plan_admission(&request, &plan)
        .expect("plan admission");
}

proptest! {
    #[test]
    fn shared_window_exact_algebra(
        input in 0_u64..1_000_000,
        reserve in 1_u64..1_000_000,
        spare in 0_u64..1_000_000,
    ) {
        let required = input + reserve;
        let total = required + spare;
        let result = evaluate_rule(
            CapacityRule::SharedWindow { total: SafeLimit::Exact(total.max(1)) },
            CountBound::Exact(input),
            Some(reserve),
        );
        prop_assert_eq!(
            result,
            Ok(ContextHeadroom::Shared(QualifiedTokens::Exact(spare)))
        );
    }

    #[test]
    fn separate_rule_checks_both_axes(
        input in 0_u64..10_000,
        reserve in 1_u64..10_000,
        input_spare in 0_u64..10_000,
        output_spare in 0_u64..10_000,
    ) {
        let result = evaluate_rule(
            CapacityRule::Separate {
                input: SafeLimit::Exact(input + input_spare),
                output: SafeLimit::GuaranteedLowerBound(reserve + output_spare),
            },
            CountBound::Exact(input),
            Some(reserve),
        );
        prop_assert_eq!(
            result,
            Ok(ContextHeadroom::Separate {
                input: QualifiedTokens::Exact(input_spare),
                output: QualifiedTokens::GuaranteedMinimum(output_spare),
            })
        );
    }

    #[test]
    fn shared_with_output_cap_reports_each_failed_axis(
        input in 1_u64..10_000,
        reserve in 1_u64..10_000,
        shared_shortfall in 1_u64..=100,
        output_shortfall in 1_u64..=100,
    ) {
        let required = input + reserve;
        let total = required.saturating_sub(shared_shortfall).max(1);
        let output = reserve.saturating_sub(output_shortfall).max(1);
        let result = evaluate_rule(
            CapacityRule::SharedWindowWithOutputCap {
                total: SafeLimit::Exact(total),
                output: SafeLimit::Exact(output),
            },
            CountBound::Exact(input),
            Some(reserve),
        );
        let Err(EvaluationFailure::Overflow(overflow)) = result else {
            prop_assert!(false, "expected overflow");
            return Ok(());
        };
        prop_assert_eq!(overflow.shared().is_some(), required > total);
        prop_assert_eq!(overflow.output().is_some(), reserve > output);
    }
}

#[test]
fn arithmetic_overflow_is_unavailable_not_wrapped() {
    assert_eq!(
        evaluate_rule(
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(u64::MAX)
            },
            CountBound::Exact(u64::MAX),
            Some(1),
        ),
        Err(EvaluationFailure::ArithmeticOverflow)
    );
}

#[test]
fn reference_client_envelope_input_is_closed_and_never_claims_output_or_exact_headroom() {
    let receipt = authority(b"reference-receipt");
    assert_eq!(
        evaluate_rule(
            CapacityRule::ProviderReservedInput {
                reported_context_window: Some(125),
                maximum_projected_input: 100,
                auto_compact_input: None,
            },
            CountBound::ReferenceClientEnvelopeUpperBound {
                tokens: 60,
                catalog_receipt: receipt,
            },
            None,
        ),
        Ok(ContextHeadroom::ProviderReservedInput {
            input: QualifiedTokens::GuaranteedMinimum(40),
        })
    );
    assert!(matches!(
        evaluate_rule(
            CapacityRule::ProviderReservedInput {
                reported_context_window: Some(125),
                maximum_projected_input: 100,
                auto_compact_input: None,
            },
            CountBound::ReferenceClientEnvelopeUpperBound {
                tokens: 101,
                catalog_receipt: receipt,
            },
            None,
        ),
        Err(EvaluationFailure::Overflow(ContextOverflow {
            input: Some(_),
            output: None,
            ..
        }))
    ));
}

#[test]
fn reference_client_envelope_receipt_drift_cannot_construct_a_contract() {
    let freshness = EvidenceFreshness::new(timestamp(10), Some(timestamp(20)))
        .expect("fresh reference evidence");
    let auth_binding = authority(b"authority");
    let capacity_receipt = authority(b"capacity-receipt");
    let capacity = CapacityCertificate::reference_client_envelope(
        &StandardIntegrationAuthority::release_owned(),
        "reference-capacity",
        "v1",
        CapacityRule::ProviderReservedInput {
            reported_context_window: Some(125),
            maximum_projected_input: 100,
            auto_compact_input: None,
        },
        auth_binding,
        freshness,
        capacity_receipt,
        "reference-v1",
    )
    .expect("reference capacity");
    let measurement = MeasurementCertificate::standard(
        &StandardIntegrationAuthority::release_owned(),
        "reference-measurement",
        "v1",
        "prepared-wire-bytes",
        "v1",
        ContextEvidenceSource::AuthenticatedDiscovery,
        CountCertificateKind::ReferenceClientEnvelopeUpperBound {
            catalog_receipt: authority(b"foreign-receipt"),
        },
        Some(auth_binding),
        freshness,
    )
    .expect("reference measurement");
    assert_eq!(
        ContextContract::new(
            ProtocolId::new("responses").expect("protocol"),
            "accounting-v1",
            "projection-v1",
            "no-output-v1",
            capacity,
            vec![measurement],
        ),
        Err(ContextContractError::EvidenceClassMismatch)
    );
}

#[test]
fn context_semantic_identity_survives_refresh_and_binds_behavior() {
    fn reference_contract(
        freshness: EvidenceFreshness,
        receipt: Digest,
        reference_revision: &str,
        reported_context_window: u64,
    ) -> ContextContract {
        let auth_binding = authority(b"semantic-authority");
        let capacity = CapacityCertificate::reference_client_envelope(
            &StandardIntegrationAuthority::release_owned(),
            "semantic-capacity",
            "capacity-v1",
            CapacityRule::ProviderReservedInput {
                reported_context_window: Some(reported_context_window),
                maximum_projected_input: 100,
                auto_compact_input: None,
            },
            auth_binding,
            freshness,
            receipt,
            reference_revision,
        )
        .expect("reference capacity");
        let measurement = MeasurementCertificate::standard(
            &StandardIntegrationAuthority::release_owned(),
            "semantic-measurement",
            "certificate-v1",
            "complete-input-bound",
            "method-v1",
            ContextEvidenceSource::AuthenticatedDiscovery,
            CountCertificateKind::ReferenceClientEnvelopeUpperBound {
                catalog_receipt: receipt,
            },
            Some(auth_binding),
            freshness,
        )
        .expect("reference measurement");
        ContextContract::new(
            ProtocolId::new("responses-family").expect("protocol family"),
            "accounting-v1",
            "projection-v1",
            "reservation-v1",
            capacity,
            vec![measurement],
        )
        .expect("context contract")
    }

    let original = reference_contract(
        EvidenceFreshness::new(timestamp(1), Some(timestamp(10))).expect("freshness"),
        authority(b"catalog-receipt-1"),
        "reference-v1",
        125,
    );
    let refreshed = reference_contract(
        EvidenceFreshness::new(timestamp(20), Some(timestamp(30))).expect("freshness"),
        authority(b"catalog-receipt-2"),
        "reference-v1",
        125,
    );
    assert_ne!(original.identity(), refreshed.identity());
    assert_eq!(original.semantic_identity(), refreshed.semantic_identity());

    let changed = reference_contract(
        EvidenceFreshness::new(timestamp(20), Some(timestamp(30))).expect("freshness"),
        authority(b"catalog-receipt-2"),
        "reference-v2",
        125,
    );
    assert_ne!(original.semantic_identity(), changed.semantic_identity());

    let changed_reported_window = reference_contract(
        EvidenceFreshness::new(timestamp(20), Some(timestamp(30))).expect("freshness"),
        authority(b"catalog-receipt-2"),
        "reference-v1",
        126,
    );
    assert_ne!(
        original.semantic_identity(),
        changed_reported_window.semantic_identity()
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end certificate test keeps the complete fail-closed matrix visible"
)]
fn reference_client_envelope_contract_requires_measurement_and_exposes_non_guarantee() {
    let freshness = EvidenceFreshness::new(timestamp(1), Some(timestamp(100)))
        .expect("fresh reference evidence");
    let auth_binding = authority_class(authority(b"reference-authority"));
    let receipt = authority(b"reference-receipt");
    let capacity = CapacityCertificate::reference_client_envelope(
        &StandardIntegrationAuthority::release_owned(),
        "reference-capacity",
        "v1",
        CapacityRule::ProviderReservedInput {
            reported_context_window: Some(125),
            maximum_projected_input: 100,
            auto_compact_input: None,
        },
        auth_binding,
        freshness,
        receipt,
        "reference-v1",
    )
    .expect("reference capacity");
    assert_eq!(
        capacity.non_guarantee_disposition(),
        Some(ContextNonGuaranteeDisposition::ReferenceClientOperatingEnvelope)
    );
    assert_eq!(
        ContextContract::new(
            ProtocolId::new("responses-family").expect("protocol"),
            "accounting-v1",
            "projection-v1",
            "no-output-v1",
            capacity.clone(),
            Vec::new(),
        ),
        Err(ContextContractError::EvidenceClassMismatch)
    );

    let measurement = MeasurementCertificate::standard(
        &StandardIntegrationAuthority::release_owned(),
        "reference-measurement",
        "v1",
        "complete-prepared-input-bound",
        "v1",
        ContextEvidenceSource::AuthenticatedDiscovery,
        CountCertificateKind::ReferenceClientEnvelopeUpperBound {
            catalog_receipt: receipt,
        },
        Some(auth_binding),
        freshness,
    )
    .expect("reference measurement");
    let reference_contract = ContextContract::new(
        ProtocolId::new("responses-family").expect("protocol"),
        "accounting-v1",
        "projection-v1",
        "no-output-v1",
        capacity,
        vec![measurement],
    )
    .expect("reference contract");
    let reference_descriptor = descriptor(
        CapacityRule::SharedWindow {
            total: SafeLimit::Exact(1),
        },
        measurement_certificate(CountCertificateKind::ExactTokenizer),
    )
    .with_context_contract(reference_contract);
    let request = request(None);
    let status = prepared_status(
        &reference_descriptor,
        &request,
        authority(b"reference-authority"),
        Some(CountObservation::new("reference-measurement", "v1", 60).expect("observation")),
        timestamp(2),
    );
    assert!(matches!(status.kind(), ContextStatusKind::Fits { .. }));
    assert!(matches!(
        status.evidence_class(),
        Some(ContextEvidenceClass::ReferenceClientEnvelope { .. })
    ));
    assert_eq!(
        status.non_guarantee_disposition(),
        Some(ContextNonGuaranteeDisposition::ReferenceClientOperatingEnvelope)
    );
    let admission = status
        .admit_with_requirement(&request, ContextAdmissionRequirement::AnyBounded)
        .expect("bounded admission");
    assert_eq!(
        admission.requirement(),
        ContextAdmissionRequirement::AnyBounded
    );
    assert_eq!(
        admission.non_guarantee_disposition(),
        Some(ContextNonGuaranteeDisposition::ReferenceClientOperatingEnvelope)
    );
    assert_eq!(
        status.admit_with_requirement(
            &request,
            ContextAdmissionRequirement::ReleaseVerifiedRequired,
        ),
        Err(ContextAdmissionError::Unavailable(
            ContextUnavailable::EvidenceRequirementNotMet {
                required: ContextAdmissionRequirement::ReleaseVerifiedRequired,
                actual: status.evidence_class().cloned(),
            }
        ))
    );
    let status_wire = serde_json::to_string(&status).expect("status wire");
    assert!(status_wire.contains("ReferenceClientOperatingEnvelope"));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end certificate test keeps the complete fail-closed matrix visible"
)]
fn caller_envelope_on_maintained_and_custom_models_is_bounded_and_never_release_verified() {
    let freshness = EvidenceFreshness::new(timestamp(1), None).expect("fresh declaration");
    let auth_binding = authority_class(authority(b"custom-authority"));
    let declaration = authority(b"custom-config-and-provenance");
    let capacity = CapacityCertificate::custom_envelope(
        "custom-capacity",
        "v1",
        CapacityRule::ProviderReservedInput {
            reported_context_window: None,
            maximum_projected_input: 100,
            auto_compact_input: None,
        },
        auth_binding,
        freshness,
        declaration,
        "custom-declaration-v1",
    )
    .expect("custom capacity");
    assert!(
        capacity
            .evidence_class()
            .allows_integration_class(IntegrationClass::Custom)
    );
    assert!(
        capacity
            .evidence_class()
            .allows_integration_class(IntegrationClass::Standard)
    );
    assert_eq!(
        capacity.non_guarantee_disposition(),
        Some(ContextNonGuaranteeDisposition::CustomCallerOperatingEnvelope)
    );
    let measurement = MeasurementCertificate::new(
        "custom-measurement",
        "v1",
        "caller-complete-input-bound",
        "v1",
        ContextEvidenceSource::CallerDeclaration,
        CountCertificateKind::CustomEnvelopeUpperBound { declaration },
        Some(auth_binding),
        freshness,
    )
    .expect("custom measurement");
    let release_capacity = CapacityCertificate::new(
        &StandardIntegrationAuthority::release_owned(),
        "release-capacity",
        "v1",
        ContextEvidenceSource::BuiltInCatalog,
        CapacityRule::SharedWindow {
            total: SafeLimit::Exact(100),
        },
        None,
        freshness,
    )
    .expect("release capacity");
    assert_eq!(
        ContextContract::new(
            ProtocolId::new("responses-family").expect("protocol"),
            "accounting-v1",
            "projection-v1",
            "reservation-v1",
            release_capacity,
            vec![measurement.clone()],
        ),
        Err(ContextContractError::EvidenceClassMismatch)
    );
    let contract = ContextContract::new(
        ProtocolId::new("responses-family").expect("protocol"),
        "accounting-v1",
        "projection-v1",
        "no-output-v1",
        capacity,
        vec![measurement],
    )
    .expect("custom contract");
    let standard = descriptor(
        CapacityRule::SharedWindow {
            total: SafeLimit::Exact(1),
        },
        measurement_certificate(CountCertificateKind::ExactTokenizer),
    );
    let custom = ResolvedModelDescriptor::new_custom(
        standard.protocol().clone(),
        standard.route().clone(),
        standard.requested_model().clone(),
        standard.reported_model().cloned(),
        standard.model_family().cloned(),
        standard.profile().clone(),
        standard.capabilities().clone(),
    )
    .with_context_contract(contract.clone());
    let maintained = standard.with_context_contract(contract);
    let request = request(None);
    for descriptor in [custom, maintained] {
        let wire = serde_json::to_vec(&descriptor).expect("descriptor wire");
        let descriptor: ResolvedModelDescriptor =
            serde_json::from_slice(&wire).expect("compatible descriptor");
        let status = prepared_status(
            &descriptor,
            &request,
            authority(b"custom-authority"),
            Some(CountObservation::new("custom-measurement", "v1", 60).expect("observation")),
            timestamp(2),
        );
        assert!(matches!(status.kind(), ContextStatusKind::Fits { .. }));
        assert!(matches!(
            status.evidence_class(),
            Some(ContextEvidenceClass::CustomEnvelope { .. })
        ));
        let admission = status
            .admit_with_requirement(&request, ContextAdmissionRequirement::AnyBounded)
            .expect("bounded admission");
        assert_eq!(
            admission.non_guarantee_disposition(),
            Some(ContextNonGuaranteeDisposition::CustomCallerOperatingEnvelope)
        );
        assert!(matches!(
            status.admit_with_requirement(
                &request,
                ContextAdmissionRequirement::ReleaseVerifiedRequired
            ),
            Err(ContextAdmissionError::Unavailable(
                ContextUnavailable::EvidenceRequirementNotMet { .. }
            ))
        ));
        let overflow = prepared_status(
            &descriptor,
            &request,
            authority(b"custom-authority"),
            Some(
                CountObservation::new("custom-measurement", "v1", 101)
                    .expect("overflow observation"),
            ),
            timestamp(2),
        );
        assert!(
            overflow
                .admit_with_requirement(&request, ContextAdmissionRequirement::AnyBounded)
                .is_err()
        );
    }
}

#[test]
fn admission_requirement_is_bound_into_the_receipt_identity() {
    let descriptor = descriptor(
        CapacityRule::SharedWindow {
            total: SafeLimit::Exact(100),
        },
        measurement_certificate(CountCertificateKind::ExactTokenizer),
    );
    let request = request(Some(20));
    let status = prepared_status(
        &descriptor,
        &request,
        authority(b"account-a"),
        Some(observation(60)),
        timestamp(2),
    );
    let any = status
        .admit_with_requirement(&request, ContextAdmissionRequirement::AnyBounded)
        .expect("bounded admission");
    let certified = status
        .admit_with_requirement(
            &request,
            ContextAdmissionRequirement::ReleaseVerifiedRequired,
        )
        .expect("certified admission");
    assert_ne!(any.identity(), certified.identity());
    assert_eq!(
        certified.requirement(),
        ContextAdmissionRequirement::ReleaseVerifiedRequired
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one adversarial matrix proves every recomputed evidence-class forgery fails"
)]
fn forged_recomputed_wire_cannot_bypass_evidence_class_invariants() {
    let freshness = EvidenceFreshness::new(timestamp(1), Some(timestamp(100))).expect("freshness");
    let authority_binding = authority(b"authority");
    let receipt = authority(b"receipt");
    let valid_capacity = CapacityCertificate::reference_client_envelope(
        &StandardIntegrationAuthority::release_owned(),
        "reference-capacity",
        "v1",
        CapacityRule::ProviderReservedInput {
            reported_context_window: Some(125),
            maximum_projected_input: 100,
            auto_compact_input: None,
        },
        authority_binding,
        freshness,
        receipt,
        "reference-v1",
    )
    .expect("capacity");

    let mut forged_capacity = serde_json::to_value(&valid_capacity).expect("capacity wire");
    forged_capacity["non_guarantee_disposition"] = serde_json::Value::Null;
    let forged_capacity_content = CapacityCertificateContent {
        evidence_id: "reference-capacity",
        revision: "v1",
        source: ContextEvidenceSource::AuthenticatedDiscovery,
        rule: CapacityRule::ProviderReservedInput {
            reported_context_window: Some(125),
            maximum_projected_input: 100,
            auto_compact_input: None,
        },
        authority_binding: Some(authority_binding),
        freshness,
        evidence_class: valid_capacity.evidence_class(),
        non_guarantee_disposition: None,
    };
    forged_capacity["identity"] = serde_json::to_value(
        semantic_digest(DigestDomain::ContextBudget, 3, &forged_capacity_content)
            .expect("forged identity"),
    )
    .expect("identity wire");
    assert!(serde_json::from_value::<CapacityCertificate>(forged_capacity).is_err());

    let valid_measurement = MeasurementCertificate::standard(
        &StandardIntegrationAuthority::release_owned(),
        "reference-measurement",
        "v1",
        "complete-input-bound",
        "v1",
        ContextEvidenceSource::AuthenticatedDiscovery,
        CountCertificateKind::ReferenceClientEnvelopeUpperBound {
            catalog_receipt: receipt,
        },
        Some(authority_binding),
        freshness,
    )
    .expect("measurement");
    let mut forged_measurement =
        serde_json::to_value(&valid_measurement).expect("measurement wire");
    forged_measurement["source"] =
        serde_json::to_value(ContextEvidenceSource::BuiltInCatalog).expect("source wire");
    forged_measurement["authority_binding"] = serde_json::Value::Null;
    let forged_measurement_content = MeasurementCertificateContent {
        certificate_id: "reference-measurement",
        revision: "v1",
        method: "complete-input-bound",
        method_revision: "v1",
        source: ContextEvidenceSource::BuiltInCatalog,
        kind: valid_measurement.kind(),
        authority_binding: None,
        freshness,
    };
    forged_measurement["identity"] = serde_json::to_value(
        semantic_digest(
            DigestDomain::TokenMeasurement,
            3,
            &forged_measurement_content,
        )
        .expect("forged identity"),
    )
    .expect("identity wire");
    assert!(serde_json::from_value::<MeasurementCertificate>(forged_measurement).is_err());

    let valid_contract = ContextContract::new(
        ProtocolId::new("responses-family").expect("protocol"),
        "accounting-v1",
        "projection-v1",
        "no-output-v1",
        valid_capacity,
        vec![valid_measurement],
    )
    .expect("contract");
    let mut forged_contract = serde_json::to_value(&valid_contract).expect("contract wire");
    forged_contract["measurements"] = serde_json::json!([]);
    let empty_measurements: Vec<MeasurementCertificate> = Vec::new();
    let forged_contract_content = ContextContractContent {
        protocol_family: valid_contract.protocol_family(),
        accounting_revision: valid_contract.accounting_revision(),
        projection_revision: valid_contract.projection_revision(),
        reservation_revision: valid_contract.reservation_revision(),
        capacity: valid_contract.capacity(),
        measurements: &empty_measurements,
    };
    forged_contract["identity"] = serde_json::to_value(
        semantic_digest(DigestDomain::ContextBudget, 4, &forged_contract_content)
            .expect("forged identity"),
    )
    .expect("identity wire");
    assert!(serde_json::from_value::<ContextContract>(forged_contract).is_err());
}

#[test]
fn standalone_freshness_and_evidence_class_deserialization_fail_closed() {
    let invalid_freshness = serde_json::json!({
        "observed_at": timestamp(10),
        "expires_at": timestamp(10),
    });
    assert!(serde_json::from_value::<EvidenceFreshness>(invalid_freshness).is_err());
    assert!(serde_json::from_str::<ContextEvidenceClass>(
            r#"{"ReferenceClientEnvelope":{"catalog_receipt":{"algorithm":"Sha256","domain":"ContextBudget","version":1,"bytes":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]},"reference_revision":""}}"#,
        )
        .is_err());
    assert!(serde_json::from_str::<ContextEvidenceClass>(r#""Certified""#).is_err());
    assert!(serde_json::from_str::<ContextEvidenceClass>(
            r#"{"ReferenceBacked":{"catalog_receipt":{"algorithm":"Sha256","domain":"ContextBudget","version":1,"bytes":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]},"reference_revision":"v1"}}"#,
        )
        .is_err());

    let capacity = CapacityCertificate::new(
        &StandardIntegrationAuthority::release_owned(),
        "release-capacity",
        "v1",
        ContextEvidenceSource::BuiltInCatalog,
        CapacityRule::SharedWindow {
            total: SafeLimit::Exact(100),
        },
        None,
        EvidenceFreshness::new(timestamp(1), None).expect("freshness"),
    )
    .expect("capacity");
    let mut mixed_wire = serde_json::to_value(capacity).expect("capacity wire");
    mixed_wire["assurance"] = serde_json::json!("Certified");
    assert!(serde_json::from_value::<CapacityCertificate>(mixed_wire).is_err());
}
