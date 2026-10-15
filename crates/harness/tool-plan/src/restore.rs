//! The plan note a compaction leaves behind.
//!
//! The task list lives in the plan tool's results, and a compaction may fold
//! those into a summary or replace them with placeholders. The model then
//! re-plans from memory, or stops. This derives the latest plan from canonical
//! history, which is durable, rather than from the in-memory plan state, which
//! is not, so the same history always yields the same note.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use plakous_model::{
    Actor, CanonicalHistory, ContentBlock, ConversationItem, ToolResultKind, ToolResultPart,
};
use plakous_tools::MERGE_PLAN_CAPABILITY_NAME;
use serde_json::Value;

use crate::text::PLAKOUS_PLAN_RESTORED_NOTE_HEADER;

/// Largest note, in bytes. Completed items go first when a plan is larger.
const MAX_PLAN_NOTE_BYTES: usize = 2 * 1024;

const TODO_ID_PREFIX: &str = " (todo id: ";

#[derive(Clone, Debug, Eq, PartialEq)]
struct NoteItem {
    status: String,
    step: String,
}

/// The note to append after a compaction turned `source` into `successor`,
/// or `None` when `source` holds no plan or `successor` still shows the same
/// one.
///
/// The latest plan is the newest successful plan tool result or earlier
/// restoration note in `source`. Items that `todo_write` created carry their
/// ids, because a merge by id needs them and the summary will not keep them.
#[must_use]
pub fn plan_restoration_note(
    source: &CanonicalHistory,
    successor: &CanonicalHistory,
) -> Option<String> {
    let plan = latest_plan(source)?;
    if latest_plan(successor).as_ref() == Some(&plan) {
        return None;
    }
    Some(render_note(&plan, &todo_ids(source)))
}

fn history_items(history: &CanonicalHistory) -> Vec<&ConversationItem> {
    history
        .segments()
        .iter()
        .flat_map(|segment| segment.items().iter())
        .collect()
}

fn latest_plan(history: &CanonicalHistory) -> Option<Vec<NoteItem>> {
    history_items(history)
        .into_iter()
        .rev()
        .find_map(|item| match item.actor() {
            Actor::Tool => item.blocks().iter().rev().find_map(|block| match block {
                ContentBlock::ToolResult(result) if result.kind() == ToolResultKind::Success => {
                    result.parts().iter().find_map(|part| match part {
                        ToolResultPart::Text(text) => parse_snapshot(text.text()),
                        _ => None,
                    })
                }
                _ => None,
            }),
            Actor::User => item.blocks().iter().find_map(|block| match block {
                ContentBlock::Text(text) => parse_note(text.text()),
                _ => None,
            }),
            _ => None,
        })
}

/// Parses the plan tools' own result text (`format_plan_snapshot`).
fn parse_snapshot(text: &str) -> Option<Vec<NoteItem>> {
    let mut lines = text.lines();
    let revision = lines.next()?.strip_prefix("revision=")?;
    if revision.is_empty() || !revision.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let mut items = Vec::new();
    for line in lines {
        if line.starts_with("explanation: ") && items.is_empty() {
            continue;
        }
        items.push(parse_item(line)?);
    }
    (!items.is_empty()).then_some(items)
}

/// Parses an earlier restoration note, so an unchanged plan is not restated.
fn parse_note(text: &str) -> Option<Vec<NoteItem>> {
    let body = text.strip_prefix(PLAKOUS_PLAN_RESTORED_NOTE_HEADER)?;
    let items = body
        .lines()
        .filter(|line| line.starts_with("- ["))
        .map(|line| {
            let mut item = parse_item(line)?;
            if let Some(start) = item.step.rfind(TODO_ID_PREFIX)
                && item.step.ends_with(')')
            {
                item.step.truncate(start);
            }
            Some(item)
        })
        .collect::<Option<Vec<_>>>()?;
    (!items.is_empty()).then_some(items)
}

fn parse_item(line: &str) -> Option<NoteItem> {
    let rest = line.strip_prefix("- [")?;
    let (status, step) = rest.split_once("] ")?;
    matches!(status, "pending" | "in_progress" | "completed").then(|| NoteItem {
        status: status.to_owned(),
        step: step.to_owned(),
    })
}

/// The id `todo_write` last gave each step text, from the calls themselves.
/// Only calls whose canonical name is the merge-plan capability count, under
/// whatever name the model called it.
fn todo_ids(history: &CanonicalHistory) -> BTreeMap<String, String> {
    let mut ids = BTreeMap::new();
    for item in history_items(history) {
        for block in item.blocks() {
            let ContentBlock::ToolCall(call) = block else {
                continue;
            };
            if call.canonical_name().as_str() != MERGE_PLAN_CAPABILITY_NAME {
                continue;
            }
            let Some(todos) = call
                .arguments()
                .and_then(|arguments| arguments.get("todos"))
                .and_then(Value::as_array)
            else {
                continue;
            };
            for todo in todos {
                let (Some(id), Some(content)) = (
                    todo.get("id").and_then(Value::as_str),
                    todo.get("content").and_then(Value::as_str),
                ) else {
                    continue;
                };
                ids.retain(|_, existing: &mut String| existing != id);
                ids.insert(content.to_owned(), id.to_owned());
            }
        }
    }
    ids
}

fn render_note(plan: &[NoteItem], ids: &BTreeMap<String, String>) -> String {
    let lines = plan
        .iter()
        .map(|item| {
            let mut line = format!("- [{}] {}", item.status, item.step);
            if let Some(id) = ids.get(&item.step) {
                let _ = write!(line, "{TODO_ID_PREFIX}{id})");
            }
            (item.status == "completed", line)
        })
        .collect::<Vec<_>>();
    let mut note = PLAKOUS_PLAN_RESTORED_NOTE_HEADER.to_owned();
    let total: usize = lines.iter().map(|(_, line)| line.len() + 1).sum();
    let keep_completed = note.len() + total <= MAX_PLAN_NOTE_BYTES;
    let mut omitted = 0_usize;
    for (completed, line) in &lines {
        // The trailing count line is short; leave room for it.
        if (*completed && !keep_completed) || note.len() + line.len() + 48 > MAX_PLAN_NOTE_BYTES {
            omitted += 1;
            continue;
        }
        note.push('\n');
        note.push_str(line);
    }
    if omitted > 0 {
        let _ = write!(note, "\n({omitted} items not shown)");
    }
    note
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "unit tests build fixtures with expect")]
mod tests {
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
}
