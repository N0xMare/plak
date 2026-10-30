//! Canonical-history calls and results with scoped process context.

use super::tool_rows::{CanonicalToolText, canonical_tool_row, settle_tool_row};
use super::{CanonicalPart, CanonicalRow, canonical_rows};
use crate::runtime::events::{
    ProcessContext, StartedToolText, completed_tool_summary, live_tool_subject,
    reasoning_visibility_admits, settled_process_wait_subject, started_tool_text,
};
use crate::sessions::projector::{ReadActor, TranscriptPart, TranscriptRead};
use crate::sessions::{ToolItem, TranscriptRow};
use plakous::agent::{ToolCallerClass, ToolCallerLineageView};
use plakous::model::Actor;
use plakous::sessions::SessionLocator;
use plakous_host_protocol::views::ProcessView;
use plakous_host_protocol::views::presentation::MAX_SNAPSHOT_ROWS;
use plakous_host_protocol::views::transcript::{Lane, RowKey, TranscriptItem};
use std::collections::HashMap;

/// A call whose result is not in committed history did not finish there.
const UNSETTLED_STATE: &str = "interrupted";

/// Maps committed items to rows, oldest first. `locator` is the session the
/// items belong to, for a process call's subject; `processes` is the job
/// registry as the host sees it now.
#[allow(
    clippy::too_many_lines,
    reason = "Canonical history projection preserves message-part order, image chips, tool lineage, hosted calls, and compaction boundaries."
)]
pub(in crate::runtime) fn canonical_items(
    items: &[TranscriptRead<'_>],
    locator: SessionLocator,
    processes: &[ProcessView],
    task_tools: bool,
) -> Vec<TranscriptItem> {
    let mut rows = KeyedRows::default();
    let mut calls: HashMap<String, Call> = HashMap::new();
    // Process commands by registry key, learnt from the registry and from
    // earlier process starts in this history, as the live path learns them.
    let mut commands: HashMap<String, String> = HashMap::new();
    for item in items {
        if rows.items.is_empty() || rows.anchor != item.position {
            rows.ordinal = 1;
        }
        // A fragment's reserved range is derived from its durable child
        // binding, even if earlier effects were omitted by the read budget.
        rows.ordinal = rows.ordinal.max(
            item.part_offset
                .saturating_mul(MAX_SNAPSHOT_ROWS as u32)
                .saturating_add(1),
        );
        rows.anchor = item.position;
        for derived in item_rows(item) {
            let index = match derived {
                CanonicalRow::Row(row) => {
                    rows.push(*row);
                    continue;
                }
                CanonicalRow::Boundary(index) => index,
            };
            match &item.parts[index] {
                TranscriptPart::Call {
                    call_id,
                    canonical,
                    exposed,
                    observation,
                    lineage,
                    composition_owner,
                } => {
                    let args =
                        serde_json::from_str::<serde_json::Value>(observation.arguments_preview())
                            .ok();
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
                        Some(*observation),
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
                    let mut tool = canonical_tool_row(
                        call_id,
                        tool,
                        label_exposed.unwrap_or(tool),
                        CanonicalToolText {
                            subject,
                            detail,
                            wait: super::tool_facts::wait_started(tool, label, args.as_ref()),
                        },
                    );
                    tool.state = UNSETTLED_STATE.to_owned();
                    tool.parent_call_id = composition_owner
                        .clone()
                        .or_else(|| lineage.parent_call_id().map(|parent| parent.to_string()));
                    tool.cell_id = row_cell_id(lineage);
                    rows.push(TranscriptRow::Tool(tool));
                }
                TranscriptPart::Result {
                    call_id,
                    succeeded,
                    preview,
                    subject,
                    lineage,
                } => {
                    let Some(mut call) = calls.remove(call_id) else {
                        continue;
                    };
                    let Some(TranscriptRow::Tool(tool)) = rows.get_mut(call.row) else {
                        continue;
                    };
                    tool.cell_id = row_cell_id(lineage).or_else(|| tool.cell_id.take());
                    if tool.parent_call_id.is_none() {
                        tool.parent_call_id =
                            lineage.parent_call_id().map(|parent| parent.to_string());
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
                TranscriptPart::Compaction => {
                    let mut tool = canonical_tool_row(
                        &format!("compaction-{}-{}", item.position, item.part_offset),
                        "compact",
                        "compact",
                        CanonicalToolText {
                            subject: None,
                            detail: None,
                            wait: None,
                        },
                    );
                    tool.state = "succeeded".to_owned();
                    rows.push(TranscriptRow::Tool(tool));
                }
                _ => {}
            }
        }
    }
    rows.items
}

fn item_rows(item: &TranscriptRead<'_>) -> Vec<CanonicalRow> {
    let actor = match item.actor {
        Actor::User => ReadActor::User,
        Actor::Assistant => ReadActor::Assistant,
        _ => ReadActor::Other,
    };
    canonical_rows(
        actor,
        item.harness_authored,
        (item.position, item.part_offset),
        None,
        item.parts.iter().map(|part| match part {
            TranscriptPart::Text(value) => CanonicalPart::Text(value),
            TranscriptPart::Image(value) => CanonicalPart::Text(value),
            TranscriptPart::Reasoning { visibility, text }
                if reasoning_visibility_admits(*visibility) =>
            {
                CanonicalPart::Reasoning(text)
            }
            TranscriptPart::Reasoning { .. } => CanonicalPart::Hidden,
            _ => CanonicalPart::Boundary,
        }),
    )
}

#[derive(Default)]
struct KeyedRows {
    items: Vec<TranscriptItem>,
    anchor: u64,
    ordinal: u32,
}

impl KeyedRows {
    fn len(&self) -> usize {
        self.items.len()
    }

    fn push(&mut self, row: TranscriptRow) {
        self.items.push(TranscriptItem {
            key: RowKey::new(self.anchor, Lane::Runtime, self.ordinal),
            row,
        });
        self.ordinal = self.ordinal.saturating_add(1);
    }

    fn get_mut(&mut self, index: usize) -> Option<&mut TranscriptRow> {
        self.items.get_mut(index).map(|item| &mut item.row)
    }
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
    settle_tool_row(tool, succeeded, preview, wait_ids);
}
