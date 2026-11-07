//! Explicit tool-row projection; shared transcript encodings remain unchanged.

use crate::views::transcript::{ToolItem, WaitFacts};
use crate::views::{ToolOperation, presentation::ActivityKind};
use serde::{Deserialize, Serialize};

/// Protocol-5 tool row, used only at the wire boundary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WireToolItem {
    /// Stable call id.
    pub call_id: String,
    /// Canonical tool name; the activity kind derives from it.
    pub name: String,
    /// The exact name the model called, present only when it differs from
    /// `name`. The row is labelled by it (chapter 01 §8.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exposed_name: Option<String>,
    /// Activity category, decided by the host from the canonical name.
    /// Serialized as `activity_kind`: `kind` is the row's own tag.
    #[serde(rename = "activity_kind")]
    pub kind: ActivityKind,
    /// Lifecycle label: `running`, `done[:elapsed]`, `failed[:reason]`,
    /// `rejected…`, `cancelled`, `interrupted`, `overflow`.
    pub state: String,
    /// One-line subject (command, path, query, …), bounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Bounded expandable detail (arguments and result preview).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Host start stamp, Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    /// Host finish stamp, Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    /// Outer local-composition marker call owning this row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_call_id: Option<String>,
    /// Local-composition cell owning this row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_id: Option<String>,
    /// Facts of a wait call: `wait_agent` or a process wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<WaitFacts>,
    /// Host-classified semantic verb; never stored or exported.
    pub operation: ToolOperation,
}

impl From<ToolItem> for WireToolItem {
    fn from(item: ToolItem) -> Self {
        let operation = item.operation();
        Self {
            operation,
            call_id: item.call_id,
            name: item.name,
            exposed_name: item.exposed_name,
            kind: item.kind,
            state: item.state,
            subject: item.subject,
            detail: item.detail,
            started_at_ms: item.started_at_ms,
            finished_at_ms: item.finished_at_ms,
            parent_call_id: item.parent_call_id,
            cell_id: item.cell_id,
            wait: item.wait,
        }
    }
}

impl From<WireToolItem> for ToolItem {
    fn from(item: WireToolItem) -> Self {
        Self {
            wire_operation: Some(item.operation),
            call_id: item.call_id,
            name: item.name,
            exposed_name: item.exposed_name,
            kind: item.kind,
            state: item.state,
            subject: item.subject,
            detail: item.detail,
            started_at_ms: item.started_at_ms,
            finished_at_ms: item.finished_at_ms,
            parent_call_id: item.parent_call_id,
            cell_id: item.cell_id,
            wait: item.wait,
        }
    }
}
