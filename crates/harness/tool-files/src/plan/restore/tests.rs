use super::*;
use plakous_model::{
    CallId, EntryId, HistorySegment, HistorySegmentKind, Provenance, TextBlock, ToolCallBlock,
    ToolCallState, ToolCorrelation, ToolExecutionOwner, ToolName, ToolResultBlock,
};

use uuid::Uuid;

fn entry(seed: u128) -> EntryId {
    EntryId::try_from(Uuid::from_u128(seed)).expect("entry")
}

fn exchange(seed: u128, arguments: &str, result: &str) -> HistorySegment {
    named_exchange(seed, "todo_write", "merge_plan", arguments, result)
}

fn named_exchange(
    seed: u128,
    exposed: &str,
    canonical: &str,
    arguments: &str,
    result: &str,
) -> HistorySegment {
    let call_id = CallId::try_from(Uuid::from_u128(seed)).expect("call");
    let call = ToolCallBlock::new(
        call_id,
        ToolName::new(exposed).expect("name"),
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::default(),
        arguments.to_owned(),
    )
    .expect("call")
    .with_canonical_name(ToolName::new(canonical).expect("canonical name"));
    let result = ToolResultBlock::new(
        call_id,
        ToolExecutionOwner::Local,
        ToolResultKind::Success,
        vec![ToolResultPart::Text(
            TextBlock::new(result, None).expect("text"),
        )],
        ToolCorrelation::default(),
    )
    .expect("result");
    HistorySegment::new(
        HistorySegmentKind::ToolExchange,
        vec![
            ConversationItem::new(
                entry(seed * 10),
                Actor::Assistant,
                vec![ContentBlock::ToolCall(call)],
                Provenance::ModelOutput,
            )
            .expect("call item"),
            ConversationItem::new(
                entry(seed * 10 + 1),
                Actor::Tool,
                vec![ContentBlock::ToolResult(result)],
                Provenance::ToolExecution,
            )
            .expect("result item"),
        ],
    )
    .expect("segment")
}

fn intent(seed: u128, text: &str) -> HistorySegment {
    HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![
            ConversationItem::new(
                entry(seed),
                Actor::User,
                vec![ContentBlock::Text(
                    TextBlock::new(text, None).expect("text"),
                )],
                Provenance::UserInput,
            )
            .expect("intent"),
        ],
    )
    .expect("segment")
}

fn history(segments: Vec<HistorySegment>) -> CanonicalHistory {
    segments
        .into_iter()
        .try_fold(CanonicalHistory::default(), |history, segment| {
            history.append(segment)
        })
        .expect("history")
}

const TODOS: &str = r#"{"merge":false,"todos":[{"id":"1","content":"read the parser","status":"completed"},{"id":"2","content":"fix the trailing comma","status":"in_progress"},{"id":"3","content":"run the tests","status":"pending"}]}"#;
const SNAPSHOT: &str = "revision=2\n- [completed] read the parser\n- [in_progress] fix the trailing comma\n- [pending] run the tests\n";

#[test]
fn a_plan_folded_away_is_restored_with_its_ids_and_statuses() {
    let source = history(vec![
        intent(1, "fix the parser"),
        exchange(2, TODOS, SNAPSHOT),
    ]);
    let successor = history(vec![intent(1, "fix the parser")]);
    let note = plan_restoration_note(&source, &successor).expect("a note");
    assert!(note.starts_with(PLAKOUS_PLAN_RESTORED_NOTE_HEADER));
    assert!(note.contains("- [in_progress] fix the trailing comma (todo id: 2)"));
    assert!(note.contains("- [pending] run the tests (todo id: 3)"));
    assert!(note.contains("- [completed] read the parser (todo id: 1)"));
    assert!(note.len() <= MAX_PLAN_NOTE_BYTES);
    // The note is itself a plan source: once it is history, the same
    // plan is not restated by the next compaction.
    assert_eq!(parse_note(&note), latest_plan(&source));
}

/// Restore binds the canonical plan capability, not the exposed name: a
/// `todo_write` alias of another tool gives no ids, and the plan tool
/// called by another name does.
#[test]
fn todo_ids_bind_the_canonical_plan_capability() {
    let source = history(vec![
        intent(1, "fix the parser"),
        named_exchange(2, "todo_write", "read_file", TODOS, "ok"),
        named_exchange(
            3,
            "plan",
            "merge_plan",
            r#"{"todos":[{"id":"7","content":"run the tests"}]}"#,
            SNAPSHOT,
        ),
    ]);
    let note = plan_restoration_note(&source, &history(Vec::new())).expect("a note");
    assert!(
        note.contains("- [pending] run the tests (todo id: 7)"),
        "{note}"
    );
    assert!(
        note.contains("- [in_progress] fix the trailing comma\n"),
        "{note}"
    );
    assert!(!note.contains("(todo id: 2)"), "{note}");
}

#[test]
fn no_plan_or_a_plan_still_visible_needs_no_note() {
    let plain = history(vec![intent(1, "hello")]);
    assert_eq!(plan_restoration_note(&plain, &plain), None);
    let source = history(vec![
        intent(1, "fix the parser"),
        exchange(2, TODOS, SNAPSHOT),
    ]);
    assert_eq!(plan_restoration_note(&source, &source), None);
    // A read that merely looks like a list is not a plan.
    let read = history(vec![
        intent(1, "read"),
        exchange(3, "{}", "- [pending] not a plan\n"),
    ]);
    assert_eq!(plan_restoration_note(&read, &plain), None);
}

#[test]
fn a_large_plan_drops_completed_items_first_and_stays_bounded() {
    let mut snapshot = String::from("revision=9\n");
    for index in 0..40 {
        let status = if index < 30 { "completed" } else { "pending" };
        let _ = writeln!(snapshot, "- [{status}] step {index} {}", "x".repeat(60));
    }
    let source = history(vec![intent(1, "plan"), exchange(4, "{}", &snapshot)]);
    let note = plan_restoration_note(&source, &history(Vec::new())).expect("a note");
    assert!(note.len() <= MAX_PLAN_NOTE_BYTES, "{} bytes", note.len());
    assert!(!note.contains("[completed]"));
    assert!(note.contains("- [pending] step 30"));
    assert!(note.contains("items not shown)"));
}
