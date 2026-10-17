#![allow(
    clippy::expect_used,
    reason = "unit tests use expect for fixture construction"
)]

use super::{TURN_CONTINUATION_NOTE_MARKER, with_turn_continuation_note};
use crate::SessionWorkflowId;
use plakous_model::{
    Actor, CallId, CanonicalHistory, ContentBlock, ConversationItem, EntryId, HistorySegment,
    HistorySegmentKind, Provenance, TextBlock, ToolCallBlock, ToolCallState, ToolCorrelation,
    ToolExecutionOwner, ToolName, ToolResultBlock, ToolResultKind, ToolResultPart,
};
use uuid::Uuid;

fn entry(value: u128) -> EntryId {
    EntryId::try_from(Uuid::from_u128(value)).expect("entry")
}

fn text_item(value: u128, actor: Actor, provenance: Provenance, text: &str) -> ConversationItem {
    ConversationItem::new(
        entry(value),
        actor,
        vec![ContentBlock::Text(
            TextBlock::new(text, None).expect("text"),
        )],
        provenance,
    )
    .expect("text item")
}

fn correlation() -> ToolCorrelation {
    ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::Direct).expect("correlation")
}

fn exchange(first_entry: u128, call: u128) -> HistorySegment {
    let call_id = CallId::try_from(Uuid::from_u128(call)).expect("call");
    HistorySegment::new(
        HistorySegmentKind::ToolExchange,
        vec![
            ConversationItem::new(
                entry(first_entry),
                Actor::Assistant,
                vec![ContentBlock::ToolCall(
                    ToolCallBlock::new(
                        call_id,
                        ToolName::new("read_file").expect("tool"),
                        ToolExecutionOwner::Local,
                        ToolCallState::Completed,
                        correlation(),
                        r#"{"target_file":"bulk.txt"}"#.to_owned(),
                    )
                    .expect("call block"),
                )],
                Provenance::ModelOutput,
            )
            .expect("call item"),
            ConversationItem::new(
                entry(first_entry + 1),
                Actor::Tool,
                vec![ContentBlock::ToolResult(
                    ToolResultBlock::new(
                        call_id,
                        ToolExecutionOwner::Local,
                        ToolResultKind::Success,
                        vec![ToolResultPart::Text(
                            TextBlock::new("page", None).expect("text"),
                        )],
                        correlation(),
                    )
                    .expect("result block"),
                )],
                Provenance::ToolExecution,
            )
            .expect("result item"),
        ],
    )
    .expect("exchange")
}

fn intent() -> HistorySegment {
    HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![text_item(
            1,
            Actor::User,
            Provenance::UserInput,
            "read bulk.txt",
        )],
    )
    .expect("intent")
}

fn workflow(byte: u8) -> SessionWorkflowId {
    SessionWorkflowId::new([byte; 32]).expect("workflow")
}

fn notes(history: &CanonicalHistory) -> Vec<usize> {
    history
        .segments()
        .iter()
        .enumerate()
        .filter(|(_, segment)| segment.kind() == HistorySegmentKind::Steering)
        .filter(|(_, segment)| {
            segment
                .items()
                .iter()
                .flat_map(ConversationItem::blocks)
                .any(|block| {
                    matches!(block, ContentBlock::Text(text)
                        if text.text().starts_with(TURN_CONTINUATION_NOTE_MARKER))
                })
        })
        .map(|(index, _)| index)
        .collect()
}

#[test]
fn a_mid_turn_successor_ends_with_one_user_role_continuation_note() {
    let successor = CanonicalHistory::new(vec![intent(), exchange(2, 20)]).expect("successor");
    let noted = with_turn_continuation_note(successor, workflow(1)).expect("noted");
    assert_eq!(notes(&noted), vec![2]);
    let segments = noted.segments();
    let item = &segments[2].items()[0];
    assert_eq!(item.actor(), Actor::User);
    assert_eq!(item.provenance(), Provenance::Instruction);
}

#[test]
fn a_later_fold_in_the_same_turn_keeps_the_one_note_instead_of_stacking() {
    let first = with_turn_continuation_note(
        CanonicalHistory::new(vec![intent(), exchange(2, 20)]).expect("first"),
        workflow(1),
    )
    .expect("first note");
    let mut segments: Vec<HistorySegment> = first.segments().iter().cloned().collect();
    segments.push(exchange(4, 21));
    let second = with_turn_continuation_note(
        CanonicalHistory::new(segments).expect("second"),
        workflow(2),
    )
    .expect("second note");
    assert_eq!(notes(&second), vec![2]);
    assert_eq!(second.segments().len(), 4);
}

#[test]
fn a_new_turn_gets_its_own_note() {
    let first = with_turn_continuation_note(
        CanonicalHistory::new(vec![intent(), exchange(2, 20)]).expect("first"),
        workflow(1),
    )
    .expect("first note");
    let mut segments: Vec<HistorySegment> = first.segments().iter().cloned().collect();
    segments.push(
        HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text_item(
                7,
                Actor::User,
                Provenance::UserInput,
                "next request",
            )],
        )
        .expect("next intent"),
    );
    segments.push(exchange(4, 21));
    let second = with_turn_continuation_note(
        CanonicalHistory::new(segments).expect("second"),
        workflow(2),
    )
    .expect("second note");
    assert_eq!(notes(&second), vec![2, 5]);
}

#[test]
fn trailing_steering_does_not_hide_the_tool_exchange_shape() {
    let plan = HistorySegment::new(
        HistorySegmentKind::Steering,
        vec![text_item(
            9,
            Actor::User,
            Provenance::Instruction,
            "plan: step 2",
        )],
    )
    .expect("plan note");
    let noted = with_turn_continuation_note(
        CanonicalHistory::new(vec![intent(), exchange(2, 20), plan]).expect("successor"),
        workflow(1),
    )
    .expect("noted");
    assert_eq!(notes(&noted), vec![3]);
}

#[test]
fn a_successor_before_a_new_prompt_gets_no_note() {
    let reply = HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![text_item(
            8,
            Actor::Assistant,
            Provenance::ModelOutput,
            "done",
        )],
    )
    .expect("reply");
    let successor =
        CanonicalHistory::new(vec![intent(), exchange(2, 20), reply]).expect("successor");
    let unchanged = with_turn_continuation_note(successor.clone(), workflow(1)).expect("noted");
    assert_eq!(unchanged.segments().len(), successor.segments().len());
    assert_eq!(notes(&unchanged), Vec::<usize>::new());
}
