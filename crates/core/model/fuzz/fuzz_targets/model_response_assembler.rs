#![no_main]

use libfuzzer_sys::fuzz_target;
use plakous_model::{
    Actor, BlockCompleted, BlockStart, BlockStarted, ContentBlock, ConversationItem,
    DataClassification, Digest, DigestDomain, EntryId, FinishDetails, FinishReason, ModelEvent,
    ModelId, ModelProviderRequestId, ModelResponse, ModelResponseAssembler, ModelResponseMetadata,
    OutputLocation, ProtocolId, Provenance, ProviderEvent, ReasoningBlock, ReasoningDelta,
    ReasoningReplayBound, ReasoningVisibility, ReasoningVisibilityBound, ReplayEnvelope,
    ReplayKindId, ReplayScope, ResponseStarted, Usage, UsageUpdate,
};
use serde_json::value::RawValue;

fuzz_target!(|data: &[u8]| {
    let Ok(digest) = Digest::sha256(DigestDomain::RequestPlan, 1, &[b"fuzz-plan"]) else {
        return;
    };
    let Ok(other) = Digest::sha256(DigestDomain::RequestPlan, 1, &[b"other-plan"]) else {
        return;
    };
    let Ok(raw) = RawValue::from_string(String::from("{}")) else {
        return;
    };
    let Ok(protocol) = ProtocolId::new("fuzz-protocol") else {
        return;
    };
    let Ok(kind) = ReplayKindId::new("reasoning-signature") else {
        return;
    };
    let Ok(item_id) = serde_json::from_str::<EntryId>("\"00000000-0000-0000-0000-000000000001\"")
    else {
        return;
    };
    let Ok(replay) = ReplayEnvelope::new(
        protocol,
        None,
        ReplayScope::Protocol,
        kind,
        1,
        DataClassification::Sensitive,
        raw.clone(),
    ) else {
        return;
    };
    let response = ModelResponse::new(
        Vec::new(),
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        digest,
    );
    let Ok(reasoning) = ReasoningBlock::new(
        ReasoningVisibility::Visible,
        Some(String::from("x")),
        Some(replay.clone()),
    ) else {
        return;
    };
    let Ok(reasoning_item) = ConversationItem::new(
        item_id,
        Actor::Assistant,
        vec![ContentBlock::Reasoning(reasoning)],
        Provenance::ModelOutput,
    ) else {
        return;
    };
    let reasoning_response = ModelResponse::new(
        vec![reasoning_item],
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        digest,
    );
    let Ok(actual_model) = ModelId::new("fuzz-actual") else {
        return;
    };
    let Ok(other_actual_model) = ModelId::new("fuzz-other") else {
        return;
    };
    let Ok(provider_request_id) = ModelProviderRequestId::new("fuzz-request") else {
        return;
    };
    let metadata =
        ModelResponseMetadata::new(Some(actual_model.clone()), Some(provider_request_id));
    let Ok(mut assembler) = ModelResponseAssembler::new(digest) else {
        return;
    };

    let mut successes = 0_usize;
    for byte in data {
        let event = match byte % 18 {
            0 => ModelEvent::ProviderEvent(ProviderEvent::new("unknown", raw.clone())),
            1 => ModelEvent::ResponseStarted(ResponseStarted::new(None, digest)),
            2 => ModelEvent::BlockStarted(BlockStarted::new(
                item_id,
                OutputLocation::new(0, 0),
                BlockStart::Reasoning {
                    visibility: ReasoningVisibility::Summary,
                    replay: None,
                },
            )),
            3 => ModelEvent::ReasoningDelta(ReasoningDelta::new(
                OutputLocation::new(0, 0),
                b"x".to_vec(),
            )),
            4 => ModelEvent::ReasoningReplayBound(ReasoningReplayBound::new(
                OutputLocation::new(0, 0),
                replay.clone(),
            )),
            5 => ModelEvent::BlockCompleted(BlockCompleted::new(OutputLocation::new(0, 0))),
            6 => ModelEvent::Usage(UsageUpdate::new(Usage::new(1, 0, 0, 0, 0))),
            7 => ModelEvent::ResponseCompleted(response.clone()),
            8 => ModelEvent::ResponseStarted(ResponseStarted::new(None, other)),
            9 => ModelEvent::ReasoningReplayBound(ReasoningReplayBound::new(
                OutputLocation::new(1, 0),
                replay.clone(),
            )),
            10 => ModelEvent::BlockStarted(BlockStarted::new(
                item_id,
                OutputLocation::new(0, 0),
                BlockStart::ReasoningPending { replay: None },
            )),
            11 => ModelEvent::ReasoningVisibilityBound(ReasoningVisibilityBound::new(
                OutputLocation::new(0, 0),
                ReasoningVisibility::Visible,
            )),
            12 => ModelEvent::ResponseCompleted(reasoning_response.clone()),
            13 => ModelEvent::ReasoningVisibilityBound(ReasoningVisibilityBound::new(
                OutputLocation::new(0, 0),
                ReasoningVisibility::Omitted,
            )),
            14 => ModelEvent::ResponseStarted(
                ResponseStarted::new(None, digest).with_metadata(metadata.clone()),
            ),
            15 => ModelEvent::ResponseCompleted(
                reasoning_response.clone().with_metadata(metadata.clone()),
            ),
            16 => ModelEvent::ResponseCompleted(reasoning_response.clone().with_metadata(
                ModelResponseMetadata::empty().with_actual_model(other_actual_model.clone()),
            )),
            _ => ModelEvent::ResponseCompleted(reasoning_response.clone().with_metadata(
                ModelResponseMetadata::empty().with_actual_model(actual_model.clone()),
            )),
        };
        if matches!(assembler.push(event), Ok(Some(_))) {
            successes = successes.saturating_add(1);
        }
        assert!(successes <= 1);
    }
});
