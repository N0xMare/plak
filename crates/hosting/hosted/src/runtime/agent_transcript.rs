//! One agent's committed history as the typed rows the root transcript shows
//! (spec 03 §16). Read from the agent's own bound session, or from a live
//! agent's atomic attach, newest first and bounded; tool calls and results are joined by call id into one tool row
//! whose subject, detail and state come from the same functions the root's
//! live path calls.

use std::collections::HashMap;

use plakous::agent::{ToolCallObservation, ToolCallerClass, ToolCallerLineageView};
use plakous::model::{Actor, ReasoningVisibility};
use plakous::sessions::{
    AgentSessions, SessionLocator, SessionTranscriptPart, SessionTranscriptSource,
    SessionTranscriptTail,
};
use plakous_host_protocol::views::ProcessView;
use plakous_host_protocol::views::presentation::{
    MAX_PRESENTATION_BYTES, MAX_SNAPSHOT_ROWS, row_bytes,
};
use plakous_host_protocol::views::transcript::ThinkingRow;

use super::events::{
    ProcessContext, StartedToolText, completed_tool_detail, completed_tool_state,
    completed_tool_summary, live_tool_subject, reasoning_visibility_admits,
    settled_process_wait_subject, started_tool_text,
};
use crate::sessions::{NoticeScope, ToolItem, TranscriptRow, bound_row};

/// A call whose result is not in committed history did not finish there.
const UNSETTLED_STATE: &str = "interrupted";

/// One presentation part, as the row mapping reads it.
pub(super) enum Part<'a> {
    Text(&'a str),
    Reasoning {
        visibility: ReasoningVisibility,
        text: &'a str,
    },
    Call {
        call_id: String,
        /// Canonical executable name: classification follows it.
        canonical: &'a str,
        /// Exact name the model called: the row is labelled by it.
        exposed: &'a str,
        observation: Option<&'a ToolCallObservation>,
        lineage: Option<&'a ToolCallerLineageView>,
        composition_owner: Option<String>,
    },
    Result {
        call_id: String,
        succeeded: bool,
        preview: &'a str,
        /// The result observation's subject, when it carried one.
        subject: Option<&'a str>,
        lineage: Option<&'a ToolCallerLineageView>,
    },
    Compaction,
}

/// One committed item, as the row mapping reads it.
pub(super) struct Item<'a> {
    pub(super) actor: Actor,
    pub(super) harness_authored: bool,
    pub(super) parts: Vec<Part<'a>>,
}

fn items(tail: &SessionTranscriptTail) -> Vec<Item<'_>> {
    tail.items()
        .iter()
        .map(|item| Item {
            actor: item.actor(),
            harness_authored: item.is_harness_authored(),
            parts: item
                .parts()
                .iter()
                .filter_map(|part| match part {
                    SessionTranscriptPart::Text(text) => Some(Part::Text(text)),
                    SessionTranscriptPart::Reasoning { visibility, text } => {
                        Some(Part::Reasoning {
                            visibility: *visibility,
                            text,
                        })
                    }
                    SessionTranscriptPart::ToolCall {
                        call_id,
                        canonical_name,
                        exposed_name,
                        observation,
                        lineage,
                    } => Some(Part::Call {
                        call_id: call_id.to_string(),
                        canonical: canonical_name.as_str(),
                        exposed: exposed_name.as_str(),
                        observation: Some(observation),
                        lineage: Some(lineage),
                        composition_owner: tail
                            .composition_owner(*call_id)
                            .filter(|owner| owner != call_id)
                            .map(|owner| owner.to_string()),
                    }),
                    SessionTranscriptPart::ToolResult {
                        call_id,
                        observation,
                        lineage,
                    } => Some(Part::Result {
                        call_id: call_id.to_string(),
                        succeeded: observation.succeeded(),
                        preview: observation.preview(),
                        subject: observation.subject(),
                        lineage: Some(lineage),
                    }),
                    SessionTranscriptPart::Compaction => Some(Part::Compaction),
                    _ => None,
                })
                .collect(),
        })
        .collect()
}

/// Maps committed items to rows, oldest first. `locator` is the session the
/// items belong to, for a process call's subject; `processes` is the job
/// registry as the host sees it now.
pub(super) fn rows_from_items(
    items: &[Item<'_>],
    locator: SessionLocator,
    processes: &[ProcessView],
    task_tools: bool,
) -> Vec<TranscriptRow> {
    let mut rows = Vec::new();
    let mut calls: HashMap<String, Call> = HashMap::new();
    // Process commands by registry key, learnt from the registry and from
    // earlier process starts in this history, as the live path learns them.
    let mut commands: HashMap<String, String> = HashMap::new();
    for (item_index, item) in items.iter().enumerate() {
        let mut text = String::new();
        for (block_index, part) in item.parts.iter().enumerate() {
            match part {
                Part::Text(value) => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(value);
                }
                Part::Reasoning {
                    visibility,
                    text: value,
                } => {
                    flush_text(&mut rows, item, &mut text);
                    if reasoning_visibility_admits(*visibility) && !value.trim().is_empty() {
                        rows.push(TranscriptRow::Thinking(ThinkingRow {
                            location_key: format!("{item_index}:{block_index}"),
                            text: (*value).to_owned(),
                            elapsed_ms: None,
                        }));
                    }
                }
                Part::Call {
                    call_id,
                    canonical,
                    exposed,
                    observation,
                    lineage,
                    composition_owner,
                } => {
                    flush_text(&mut rows, item, &mut text);
                    let args = observation.and_then(|observation| {
                        serde_json::from_str::<serde_json::Value>(observation.arguments_preview())
                            .ok()
                    });
                    // Read exactly as a live row reads its audit identity:
                    // presentation renames the canonical call, and a renamed
                    // call keeps no exposed name.
                    let tool: &str = plakous_host_protocol::topology::presented_slot_name(
                        canonical,
                        args.as_ref(),
                        task_tools,
                    );
                    let label_exposed = (tool == *canonical).then_some(*exposed);
                    let label = args.as_ref().and_then(|args| {
                        super::tools::process_observation_label(tool, label_exposed, args)
                    });
                    if let (Some(_), Some(args)) = (label, args.as_ref()) {
                        for id in super::tools::process_ids(args) {
                            let key = super::tools::process_key(locator, &id);
                            if let Some(process) = processes
                                .iter()
                                .find(|process| process.id == key && !process.command.is_empty())
                            {
                                commands.insert(key, process.command.clone());
                            }
                        }
                    }
                    let StartedToolText { subject, detail } = started_tool_text(
                        tool,
                        label_exposed,
                        *observation,
                        None,
                        ProcessContext {
                            label,
                            args: args.as_ref(),
                            locator: Some(locator),
                            processes,
                            commands: &commands,
                        },
                    );
                    let process = match (label, args.as_ref()) {
                        (Some("process_session_wait"), Some(args)) => {
                            Some(CallProcess::Wait(super::tools::process_ids(args)))
                        }
                        (Some("Shell"), Some(args)) => Some(CallProcess::Start(
                            super::tools::tool_subject_from_args("process_session_start", args)
                                .unwrap_or_default(),
                        )),
                        _ => None,
                    };
                    // Only its result tells whether a pi `wait` ran the await.
                    let wait_args = (task_tools && tool == "wait")
                        .then(|| args.clone())
                        .flatten();
                    calls.insert(
                        call_id.clone(),
                        Call {
                            row: rows.len(),
                            process,
                            wait_args,
                        },
                    );
                    rows.push(TranscriptRow::Tool(ToolItem {
                        call_id: call_id.clone(),
                        name: (*tool).to_owned(),
                        exposed_name: label_exposed
                            .filter(|exposed| *exposed != tool)
                            .map(str::to_owned),
                        kind: crate::activity::tool_activity_kind(tool),
                        state: UNSETTLED_STATE.to_owned(),
                        subject,
                        detail,
                        started_at_ms: None,
                        finished_at_ms: None,
                        parent_call_id: composition_owner.clone().or_else(|| {
                            lineage.and_then(|lineage| {
                                lineage.parent_call_id().map(|parent| parent.to_string())
                            })
                        }),
                        cell_id: lineage.and_then(row_cell_id),
                        wait: super::tool_facts::wait_started(tool, label, args.as_ref()),
                    }));
                }
                Part::Result {
                    call_id,
                    succeeded,
                    preview,
                    subject,
                    lineage,
                } => {
                    flush_text(&mut rows, item, &mut text);
                    let Some(mut call) = calls.remove(call_id) else {
                        continue;
                    };
                    let Some(TranscriptRow::Tool(tool)) = rows.get_mut(call.row) else {
                        continue;
                    };
                    if let Some(lineage) = lineage {
                        tool.cell_id = row_cell_id(lineage).or_else(|| tool.cell_id.take());
                        if tool.parent_call_id.is_none() {
                            tool.parent_call_id =
                                lineage.parent_call_id().map(|parent| parent.to_string());
                        }
                    }
                    if *succeeded {
                        settle_bare_await(tool, &mut call, preview, *subject, task_tools);
                    }
                    let wait_ids = match &call.process {
                        Some(CallProcess::Wait(ids)) => Some(ids.as_slice()),
                        _ => None,
                    };
                    settle(tool, *succeeded, preview, wait_ids);
                    match call.process {
                        Some(CallProcess::Wait(ids)) => {
                            tool.subject = Some(settled_process_wait_subject(
                                &ids,
                                Some(preview),
                                Some(locator),
                                processes,
                                &commands,
                            ));
                        }
                        Some(CallProcess::Start(command)) if !command.is_empty() => {
                            if let Ok(result) = serde_json::from_str::<serde_json::Value>(preview) {
                                for id in super::tools::process_ids(&result) {
                                    commands.insert(
                                        super::tools::process_key(locator, &id),
                                        command.clone(),
                                    );
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Part::Compaction => {
                    flush_text(&mut rows, item, &mut text);
                    rows.push(TranscriptRow::Tool(ToolItem {
                        call_id: format!("compaction-{item_index}"),
                        name: "compact".to_owned(),
                        exposed_name: None,
                        kind: crate::activity::tool_activity_kind("compact"),
                        state: "succeeded".to_owned(),
                        subject: None,
                        detail: None,
                        started_at_ms: None,
                        finished_at_ms: None,
                        parent_call_id: None,
                        cell_id: None,
                        wait: None,
                    }));
                }
            }
        }
        flush_text(&mut rows, item, &mut text);
    }
    rows
}

// A cell id without a parent denotes a marker/yield to the renderer, not an
// ordinary nested call whose owning receipt is unavailable.
fn row_cell_id(lineage: &ToolCallerLineageView) -> Option<String> {
    if lineage.parent_call_id().is_none()
        && (lineage.nested_ordinal().is_some()
            || lineage.class() == ToolCallerClass::ProviderProgrammatic)
    {
        return None;
    }
    lineage.cell_id().map(str::to_owned)
}

/// A call row waiting for its result, and what its process facts need.
struct Call {
    row: usize,
    process: Option<CallProcess>,
    /// A pi `wait`'s arguments on a node granted agent tools.
    wait_args: Option<serde_json::Value>,
}

/// A bare pi `wait` whose result is the tree's await receipt reads as the
/// await, as it did live: its name, kind, ceiling and child list, and no
/// process facts. `subject` is the result observation's own field. The
/// preview is only a fallback, and a preview past 2 KiB may no longer
/// contain the agent ids.
fn settle_bare_await(
    tool: &mut ToolItem,
    call: &mut Call,
    preview: &str,
    subject: Option<&str>,
    task_tools: bool,
) {
    let result = serde_json::from_str::<serde_json::Value>(preview).ok();
    let name = plakous_host_protocol::topology::presented_settled_name(
        &tool.name,
        call.wait_args.as_ref(),
        result.as_ref(),
        task_tools,
    );
    if name == tool.name {
        return;
    }
    let name = name.to_owned();
    tool.kind = crate::activity::tool_activity_kind(&name);
    tool.name = name;
    tool.wait = Some(super::tool_facts::bare_await_started(tool.wait.take()));
    tool.subject = subject
        .map(str::to_owned)
        .or_else(|| result.as_ref().and_then(super::tool_facts::await_children))
        .map(|children| live_tool_subject(&children));
    call.process = None;
}

enum CallProcess {
    /// A process wait on these ids.
    Wait(Vec<String>),
    /// A process start running this command.
    Start(String),
}

fn settle(tool: &mut ToolItem, succeeded: bool, preview: &str, wait_ids: Option<&[String]>) {
    if let Some(subject) =
        completed_tool_summary(&tool.name, tool.subject.as_deref(), Some(preview))
    {
        tool.subject = Some(live_tool_subject(&subject));
    }
    if let Some(detail) = completed_tool_detail(tool.detail.as_deref(), Some(preview)) {
        tool.detail = Some(detail);
    }
    tool.state = completed_tool_state(&tool.name, Some((succeeded, preview)));
    let result = serde_json::from_str::<serde_json::Value>(preview).ok();
    tool.wait = super::tool_facts::wait_settled(
        &tool.name,
        tool.wait.take(),
        result.as_ref(),
        succeeded,
        None,
        wait_ids,
    );
}

/// Inbound mail is one tree notice, a prompt's child-mail batch one per
/// child; a harness note is not the operator's.
fn flush_text(rows: &mut Vec<TranscriptRow>, item: &Item<'_>, text: &mut String) {
    if text.trim().is_empty() {
        text.clear();
        return;
    }
    let value = std::mem::take(text);
    if let Some(mail) = crate::topology_navigator::mail_row(&value) {
        rows.push(TranscriptRow::info(NoticeScope::Tree, mail));
        return;
    }
    if item.harness_authored {
        return;
    }
    match item.actor {
        Actor::User => {
            let (own, mail) = crate::topology_navigator::split_child_mail(&value)
                .unwrap_or_else(|| (value, Vec::new()));
            if !own.trim().is_empty() {
                rows.push(TranscriptRow::user(own));
            }
            rows.extend(
                mail.into_iter()
                    .map(|mail| TranscriptRow::info(NoticeScope::Tree, mail)),
            );
        }
        Actor::Assistant => rows.push(TranscriptRow::assistant(value)),
        _ => {}
    }
}

/// Bounds every row like a stored row, then keeps the newest rows that fit
/// `max_rows` and `max_bytes`, behind one `Truncated` row when anything older
/// was dropped here or by the read. The marker takes a row and its bytes only
/// when something is dropped: rows that exactly fit are all kept.
pub(super) fn bound_rows(
    rows: Vec<TranscriptRow>,
    mut truncated: bool,
    max_rows: usize,
    max_bytes: usize,
) -> Vec<TranscriptRow> {
    if max_rows == 0 {
        return Vec::new();
    }
    let mut kept: Vec<(TranscriptRow, usize)> = Vec::new();
    let mut bytes = 0_usize;
    for row in rows.into_iter().rev().map(bound_row) {
        let size = row_bytes(&row);
        if kept.len() >= max_rows || bytes.saturating_add(size) > max_bytes {
            truncated = true;
            break;
        }
        bytes = bytes.saturating_add(size);
        kept.push((row, size));
    }
    let mut kept = if truncated {
        let marker = row_bytes(&TranscriptRow::Truncated);
        while kept.len() >= max_rows || bytes.saturating_add(marker) > max_bytes {
            let Some((_, size)) = kept.pop() else {
                break;
            };
            bytes = bytes.saturating_sub(size);
        }
        let mut kept: Vec<TranscriptRow> = kept.into_iter().map(|(row, _)| row).collect();
        kept.push(TranscriptRow::Truncated);
        kept
    } else {
        kept.into_iter().map(|(row, _)| row).collect()
    };
    kept.reverse();
    // A composition parent outside the window cannot own a visible row.
    let calls: std::collections::BTreeSet<String> = kept
        .iter()
        .filter_map(|row| row.call_id().map(str::to_owned))
        .collect();
    for row in &mut kept {
        if let TranscriptRow::Tool(tool) = row
            && tool
                .parent_call_id
                .as_ref()
                .is_some_and(|parent| !calls.contains(parent))
        {
            tool.parent_call_id = None;
            tool.cell_id = None;
        }
    }
    kept
}

/// The most committed items one read projects. Each item becomes at most a
/// few rows, so 512 items cover the last few hundred calls and messages of an
/// agent, which is what a reader scrolls through in a view; anything older is
/// behind the `Truncated` row and still readable by call detail. Each
/// argument or result string is redacted whole up to 64 KiB and past that over
/// the agent library's history window (`HISTORY_WINDOW_BYTES`, 20 KiB), so one
/// read's work stays in the tens of MiB however long the history is.
pub(super) const MAX_AGENT_TRANSCRIPT_ITEMS: usize = 512;

/// Projects `source`'s newest items to bounded rows. Synchronous CPU work:
/// callers run it off the async workers.
pub(super) fn rows_from_source(
    source: &SessionTranscriptSource,
    processes: &[ProcessView],
    task_tools: bool,
) -> Vec<TranscriptRow> {
    rows_within(
        source,
        processes,
        task_tools,
        MAX_AGENT_TRANSCRIPT_ITEMS,
        MAX_SNAPSHOT_ROWS,
        MAX_PRESENTATION_BYTES,
    )
}

fn rows_within(
    source: &SessionTranscriptSource,
    processes: &[ProcessView],
    task_tools: bool,
    max_items: usize,
    max_rows: usize,
    max_bytes: usize,
) -> Vec<TranscriptRow> {
    let tail = source.tail(max_items, max_bytes);
    let rows = rows_from_items(&items(&tail), tail.locator(), processes, task_tools);
    bound_rows(rows, tail.truncated(), max_rows, max_bytes)
}

/// Reads `locator`'s committed history at the current head of its session
/// and branch, never another's.
pub(super) async fn read_source(
    sessions: &AgentSessions,
    locator: SessionLocator,
) -> Result<SessionTranscriptSource, String> {
    sessions
        .transcript_source(locator)
        .await
        .map_err(|error| format!("transcript unavailable: {error}"))
}

#[cfg(test)]
pub(super) async fn read_rows(
    sessions: &AgentSessions,
    locator: SessionLocator,
    processes: &[ProcessView],
) -> Result<Vec<TranscriptRow>, String> {
    Ok(rows_from_source(
        &read_source(sessions, locator).await?,
        processes,
        false,
    ))
}

#[cfg(test)]
pub(super) async fn read_rows_within(
    sessions: &AgentSessions,
    locator: SessionLocator,
    processes: &[ProcessView],
    max_rows: usize,
    max_bytes: usize,
) -> Result<Vec<TranscriptRow>, String> {
    Ok(rows_within(
        &read_source(sessions, locator).await?,
        processes,
        false,
        max_rows.min(MAX_AGENT_TRANSCRIPT_ITEMS),
        max_rows,
        max_bytes,
    ))
}

#[cfg(test)]
mod tests;
