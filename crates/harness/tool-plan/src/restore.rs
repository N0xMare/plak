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
mod tests;
