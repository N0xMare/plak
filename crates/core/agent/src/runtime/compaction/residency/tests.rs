#![allow(
    clippy::expect_used,
    reason = "Image policy fixtures name the exact constructor that failed"
)]

use super::*;
use crate::SessionWorkflowId;
use plakous_model::{
    CompactionBlock, EntryId, MediaSource, Provenance, TextBlock, ToolCallBlock, ToolCallState,
    ToolCallerLineage, ToolCorrelation, ToolExecutionOwner, ToolName, ToolResultBlock,
    ToolResultKind,
};
use uuid::Uuid;

fn image() -> ImageBlock {
    ImageBlock::new(
        "image/png",
        MediaSource::bytes(vec![1; 300]).expect("bytes"),
    )
    .expect("image")
}
fn segment(
    id: u128,
    kind: HistorySegmentKind,
    actor: Actor,
    blocks: Vec<ContentBlock>,
) -> HistorySegment {
    HistorySegment::new(
        kind,
        vec![
            ConversationItem::new(
                EntryId::try_from(Uuid::from_u128(id)).expect("entry"),
                actor,
                blocks,
                if actor == Actor::User {
                    Provenance::UserInput
                } else {
                    Provenance::ModelOutput
                },
            )
            .expect("item"),
        ],
    )
    .expect("segment")
}
fn images(count: usize) -> Vec<ContentBlock> {
    (0..count).map(|_| ContentBlock::Image(image())).collect()
}
fn text() -> ContentBlock {
    ContentBlock::Text(TextBlock::new("consumed", None).expect("text"))
}
fn fixture(old: usize, latest: usize) -> CanonicalHistory {
    CanonicalHistory::new(vec![
        segment(
            1,
            HistorySegmentKind::RecentIntent,
            Actor::User,
            images(old),
        ),
        segment(
            2,
            HistorySegmentKind::Conversation,
            Actor::Assistant,
            vec![text()],
        ),
        segment(
            3,
            HistorySegmentKind::RecentIntent,
            Actor::User,
            images(latest),
        ),
    ])
    .expect("history")
}
fn incoming(count: u32) -> ImageTotals {
    ImageTotals {
        count,
        encoded_bytes: u64::from(count) * 400,
        unknown_bytes: 0,
    }
}
fn workflow() -> SessionWorkflowId {
    SessionWorkflowId::new([7; 32]).expect("workflow")
}

#[test]
fn byte_target_stops_exactly_at_seventy_five_percent() {
    let source = fixture(21, 1);
    let plan = evaluate(
        &source,
        ImageTotals::ZERO,
        ResidentImageBudget::new(100, 3_200).expect("budget"),
    )
    .expect("policy")
    .expect("plan");
    assert_eq!(plan.after.encoded_bytes, 2_400);
    assert_eq!(plan.after.count, 6);
}

#[test]
fn retiring_uri_images_decrements_unknown_bytes_and_recomputes_totals() {
    let uri = ImageBlock::new(
        "image/png",
        MediaSource::uri("https://example.invalid/image.png").expect("URI"),
    )
    .expect("image");
    let source = CanonicalHistory::new(vec![
        segment(
            1,
            HistorySegmentKind::RecentIntent,
            Actor::User,
            vec![ContentBlock::Image(uri); 21],
        ),
        segment(2, HistorySegmentKind::RecentIntent, Actor::User, images(1)),
    ])
    .expect("history");
    let plan = evaluate(&source, ImageTotals::ZERO, ResidentImageBudget::default())
        .expect("policy")
        .expect("plan");
    assert_eq!(plan.after.unknown_bytes, 14);
    let successor = residency_successor(&source, &plan, workflow()).expect("successor");
    assert_eq!(
        image_totals(&successor.history, ImageTotals::ZERO),
        plan.after
    );
}

#[test]
fn user_steering_with_retirement_markers_is_never_a_harness_note() {
    for marker in [
        rewrite::NOTE_MARKER,
        crate::runtime::compaction::placeholder::PLACEHOLDER_HARNESS_NOTE_MARKER,
    ] {
        let steering = segment(
            90,
            HistorySegmentKind::Steering,
            Actor::User,
            vec![ContentBlock::Text(
                TextBlock::new(format!("{marker} user instruction"), None).expect("text"),
            )],
        );
        let mut segments = fixture(21, 1).segments().to_vec();
        segments.push(steering.clone());
        let source = CanonicalHistory::new(segments).expect("history");
        let plan = evaluate(&source, ImageTotals::ZERO, ResidentImageBudget::default())
            .expect("policy")
            .expect("plan");
        let successor = residency_successor(&source, &plan, workflow()).expect("successor");
        assert!(
            successor
                .history
                .segments()
                .iter()
                .any(|segment| segment == &steering)
        );
    }
}

#[test]
fn budget_selects_oldest_images_until_both_axes_reach_seventy_five_percent() {
    let history = fixture(21, 1);
    let plan = evaluate(&history, ImageTotals::ZERO, ResidentImageBudget::default())
        .expect("policy")
        .expect("plan");
    assert_eq!(plan.selection.len(), 7);
    assert_eq!(plan.after.count, 15);
    assert_eq!(
        plan.selection
            .iter()
            .map(|image| image.block)
            .collect::<Vec<_>>(),
        (0..7).collect::<Vec<_>>()
    );
    let byte_budget = ResidentImageBudget::new(100, 4_000).expect("budget");
    let plan = evaluate(&history, ImageTotals::ZERO, byte_budget)
        .expect("policy")
        .expect("plan");
    assert!(plan.after.encoded_bytes <= 3_000);
}

#[test]
fn maximum_legal_batch_is_accepted_above_target_when_it_fits_full_budget() {
    let history = fixture(7, 6);
    let plan = evaluate(&history, incoming(8), ResidentImageBudget::default())
        .expect("policy")
        .expect("plan");
    assert_eq!(
        (plan.before.count, plan.selection.len(), plan.after.count),
        (21, 5, 16)
    );
    assert!(plan.validate_source(&history).is_ok());
    let successor = residency_successor(&history, &plan, workflow()).expect("successor");
    assert_eq!(image_totals(&successor.history, incoming(8)), plan.after);
    assert_eq!(successor.history.segments()[3], history.segments()[2]);
}

#[test]
fn maximum_legal_batch_refuses_if_protected_residual_exceeds_full_budget() {
    let history = fixture(3, 13);
    let error = evaluate(&history, incoming(8), ResidentImageBudget::default())
        .expect_err("protected residual");
    assert_eq!(error.kind(), AgentErrorKind::RequestBudget);
    assert!(error.message().contains("23 images"));
}

#[test]
fn recovery_recomputation_requires_exact_selection_metadata_order_and_totals() {
    let history = fixture(7, 6);
    let plan = evaluate(&history, incoming(8), ResidentImageBudget::default())
        .expect("policy")
        .expect("plan");
    let mut variants = Vec::new();
    let mut changed = plan.clone();
    changed.selection.pop();
    variants.push(changed);
    let mut changed = plan.clone();
    changed.selection.swap(0, 1);
    variants.push(changed);
    let mut changed = plan.clone();
    changed.selection[0].sha256 = None;
    variants.push(changed);
    let mut changed = plan.clone();
    changed.selection[0].block = 6;
    variants.push(changed);
    let mut changed = plan.clone();
    changed.after.encoded_bytes += 1;
    variants.push(changed);
    let mut changed = plan.clone();
    changed.policy_revision = 2;
    variants.push(changed);
    for changed in variants {
        assert!(changed.validate_source(&history).is_err(), "{changed:?}");
    }
    assert!(plan.validate_source(&history).is_ok());
}

#[test]
fn raw_data_url_and_remote_accounting_do_not_confuse_unknown_and_zero() {
    assert_eq!(
        MediaSource::bytes(vec![1])
            .expect("bytes")
            .encoded_image_bytes(),
        Some(4)
    );
    assert_eq!(
        MediaSource::bytes(vec![1; 4])
            .expect("bytes")
            .encoded_image_bytes(),
        Some(8)
    );
    assert_eq!(
        MediaSource::data_url("data:image/png;base64,YQ==")
            .expect("data")
            .encoded_image_bytes(),
        Some(4)
    );
    assert_eq!(
        MediaSource::uri("https://example.invalid/image")
            .expect("uri")
            .encoded_image_bytes(),
        None
    );
    let mut totals = ImageTotals::ZERO;
    totals.add_image(
        &ImageBlock::new(
            "image/png",
            MediaSource::uri("https://example.invalid/image").expect("uri"),
        )
        .expect("image"),
    );
    assert_eq!(
        totals,
        ImageTotals {
            count: 1,
            encoded_bytes: 0,
            unknown_bytes: 1
        }
    );
}

#[test]
fn steering_and_latest_segments_survive_rewrite_and_only_one_note_remains() {
    let source = fixture(21, 1);
    let mut segments = source.segments().to_vec();
    let steering = segment(4, HistorySegmentKind::Steering, Actor::User, images(1));
    segments.push(steering.clone());
    let source = CanonicalHistory::new(segments).expect("history");
    let plan = evaluate(&source, ImageTotals::ZERO, ResidentImageBudget::default())
        .expect("policy")
        .expect("plan");
    let successor = residency_successor(&source, &plan, workflow()).expect("rewrite");
    assert!(
        successor
            .history
            .segments()
            .iter()
            .any(|segment| segment == &steering)
    );
    assert!(
        successor
            .history
            .segments()
            .iter()
            .any(|segment| segment == &source.segments()[2])
    );
    let text = serde_json::to_string(&successor.history).expect("encoding");
    assert!(text.contains("This is not the image."));
    assert!(!text.contains("Before continuing, write down"));
    assert_eq!(text.matches("[plak: older context retired]").count(), 1);
    assert_eq!(
        successor
            .history
            .segments()
            .iter()
            .filter(|segment| segment.kind() == HistorySegmentKind::Compaction)
            .count(),
        1
    );
    assert!(
        evaluate(
            &successor.history,
            ImageTotals::ZERO,
            ResidentImageBudget::default()
        )
        .expect("no repeat")
        .is_none()
    );
}

#[test]
fn native_compaction_segment_is_retained_byte_exact() {
    let source = fixture(21, 1);
    let native = segment(
        90,
        HistorySegmentKind::Compaction,
        Actor::Assistant,
        vec![ContentBlock::Compaction(
            CompactionBlock::new("native summary")
                .expect("summary")
                .with_native_item(
                    r#"{"type":"compaction","encrypted_content":"fixture-only-opaque"}"#.to_owned(),
                ),
        )],
    );
    let mut segments = source.segments().to_vec();
    segments.insert(0, native.clone());
    let source = CanonicalHistory::new(segments).expect("history");
    let plan = evaluate(&source, ImageTotals::ZERO, ResidentImageBudget::default())
        .expect("policy")
        .expect("plan");
    let successor = residency_successor(&source, &plan, workflow()).expect("rewrite");
    assert_eq!(successor.notice.canonical_bytes(), native.canonical_bytes());
    assert_eq!(
        successor.history.segments()[0].canonical_bytes(),
        native.canonical_bytes()
    );
    assert_ne!(successor.history.identity(), source.identity());
}

#[test]
fn tool_image_rewrite_preserves_calls_text_parts_pairing_and_unanswered_batch() {
    let correlation =
        ToolCorrelation::new(None, None, ToolCallerLineage::Direct).expect("correlation");
    let call_id = CallId::try_from(Uuid::from_u128(50)).expect("call id");
    let call = ToolCallBlock::new(
        call_id,
        ToolName::new("view_image").expect("tool"),
        ToolExecutionOwner::Local,
        ToolCallState::Completed,
        correlation.clone(),
        "{\"path\":\"image.png\"}",
    )
    .expect("call");
    let parts = std::iter::once(ToolResultPart::Text(
        TextBlock::new("keep this text", None).expect("text"),
    ))
    .chain((0..21).map(|_| ToolResultPart::Image(image())))
    .collect();
    let result = ToolResultBlock::new(
        call_id,
        ToolExecutionOwner::Local,
        ToolResultKind::Success,
        parts,
        correlation,
    )
    .expect("result");
    let call_item = ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(51)).expect("entry"),
        Actor::Assistant,
        vec![ContentBlock::ToolCall(call.clone())],
        Provenance::ModelOutput,
    )
    .expect("item");
    let result_item = ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(52)).expect("entry"),
        Actor::Tool,
        vec![ContentBlock::ToolResult(result)],
        Provenance::ToolExecution,
    )
    .expect("item");
    let batch = HistorySegment::new(
        HistorySegmentKind::ToolExchange,
        vec![call_item, result_item],
    )
    .expect("batch");
    let intent = segment(
        1,
        HistorySegmentKind::RecentIntent,
        Actor::User,
        vec![text()],
    );
    let unanswered = CanonicalHistory::new(vec![intent.clone(), batch.clone()]).expect("history");
    assert!(
        evaluate(
            &unanswered,
            ImageTotals::ZERO,
            ResidentImageBudget::default()
        )
        .is_err()
    );
    let source = CanonicalHistory::new(vec![
        intent,
        batch,
        segment(
            3,
            HistorySegmentKind::Conversation,
            Actor::Assistant,
            vec![text()],
        ),
    ])
    .expect("history");
    let plan = evaluate(&source, ImageTotals::ZERO, ResidentImageBudget::default())
        .expect("policy")
        .expect("plan");
    let successor = residency_successor(&source, &plan, workflow()).expect("rewrite");
    let segments = successor.history.segments();
    let items = segments[2].items();
    assert!(matches!(&items[0].blocks()[0], ContentBlock::ToolCall(retained) if retained == &call));
    let ContentBlock::ToolResult(result) = &items[1].blocks()[0] else {
        panic!("result");
    };
    assert!(
        matches!(&result.parts()[0], ToolResultPart::Text(text) if text.text() == "keep this text")
    );
    assert_eq!(result.call_id(), call_id);
    assert!(
        matches!(&result.parts()[1], ToolResultPart::Text(text) if text.text().contains("check its effects"))
    );
}

#[test]
fn existing_semantic_summary_is_preserved_and_the_notice_remains_single() {
    let source = fixture(21, 1);
    let mut segments = source.segments().to_vec();
    let summary = ContentBlock::Compaction(CompactionBlock::new("keep summary").expect("summary"));
    segments.insert(
        0,
        segment(
            80,
            HistorySegmentKind::Compaction,
            Actor::Assistant,
            vec![summary],
        ),
    );
    let source = CanonicalHistory::new(segments).expect("history");
    let plan = evaluate(&source, ImageTotals::ZERO, ResidentImageBudget::default())
        .expect("policy")
        .expect("plan");
    assert!(plan.selection.iter().all(|selected| selected.segment != 0));
    let successor = residency_successor(&source, &plan, workflow()).expect("rewrite");
    assert!(
        serde_json::to_string(&successor.history)
            .expect("encoding")
            .contains("keep summary")
    );
}

#[test]
fn a_consumed_tool_image_is_a_residency_candidate_like_a_user_image() {
    let correlation =
        ToolCorrelation::new(None, None, ToolCallerLineage::Direct).expect("correlation");
    let call_id = CallId::try_from(Uuid::from_u128(70)).expect("call id");
    let parts = (0..3).map(|_| ToolResultPart::Image(image())).collect();
    let result = ToolResultBlock::new(
        call_id,
        ToolExecutionOwner::Local,
        ToolResultKind::Success,
        parts,
        correlation.clone(),
    )
    .expect("result");
    let call = ToolCallBlock::new(
        call_id,
        ToolName::new("view_image").expect("tool"),
        ToolExecutionOwner::Local,
        ToolCallState::Completed,
        correlation,
        "{\"path\":\"shot.png\"}",
    )
    .expect("call");
    let batch = HistorySegment::new(
        HistorySegmentKind::ToolExchange,
        vec![
            ConversationItem::new(
                EntryId::try_from(Uuid::from_u128(71)).expect("entry"),
                Actor::Assistant,
                vec![ContentBlock::ToolCall(call)],
                Provenance::ModelOutput,
            )
            .expect("call item"),
            ConversationItem::new(
                EntryId::try_from(Uuid::from_u128(72)).expect("entry"),
                Actor::Tool,
                vec![ContentBlock::ToolResult(result)],
                Provenance::ToolExecution,
            )
            .expect("result item"),
        ],
    )
    .expect("batch");
    let source = CanonicalHistory::new(vec![
        batch,
        segment(
            73,
            HistorySegmentKind::Conversation,
            Actor::Assistant,
            vec![text()],
        ),
        segment(
            74,
            HistorySegmentKind::RecentIntent,
            Actor::User,
            vec![ContentBlock::Image(image())],
        ),
    ])
    .expect("history");
    let plan = evaluate(
        &source,
        ImageTotals::ZERO,
        ResidentImageBudget::new(3, 16 * 1024 * 1024).expect("budget"),
    )
    .expect("policy")
    .expect("plan");
    assert!(
        plan.selection
            .iter()
            .any(|selected| selected.kind == ImageSelectionKind::ToolResultImage)
    );
    assert!(
        plan.selection
            .iter()
            .all(|selected| selected.kind != ImageSelectionKind::UserImage)
    );
}
