//! JSONL protocol between `plakous-js-host` and its parent.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::RuntimeEvent;

/// Default cooperative yield interval (`CODEX_NATIVE_CODE_MODE_DEFAULT_YIELD_MS_V1`).
pub const DEFAULT_YIELD_TIME_MS: u64 = 10_000;

const fn default_yield_time_ms() -> u64 {
    DEFAULT_YIELD_TIME_MS
}

/// Parent → worker.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ClientOp {
    /// Start one cell.
    Start {
        execution_id: u64,
        source: String,
        tools: Vec<Value>,
        #[serde(default)]
        stored: HashMap<String, Value>,
        #[serde(default = "default_yield_time_ms")]
        yield_time_ms: u64,
    },
    /// Nested tool result.
    ToolResult {
        execution_id: u64,
        id: u64,
        value: Value,
        success: bool,
    },
    /// Resume a parked `yield_control()` or auto-yield.
    Resume {
        execution_id: u64,
        #[serde(default = "default_yield_time_ms")]
        yield_time_ms: u64,
    },
    /// Cooperative interrupt.
    Interrupt,
    /// Stop the worker.
    Shutdown,
}

/// Worker → parent.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum HostOp {
    /// Exact result was accepted by this interpreter call.
    ToolResultAccepted {
        execution_id: u64,
        id: u64,
    },
    /// The result named no pending request (unknown id or foreign cell); the
    /// cell is undisturbed and this is not a host loss.
    ToolResultRefused {
        execution_id: u64,
        id: u64,
        message: String,
    },
    /// The exact result provably never reached the evaluator: the worker
    /// refused or failed before the result was handed to the interpreter. The
    /// cell never observed it, so the outcome is definite and no recovery is
    /// owed. Distinct from `HostLost`, which leaves acceptance undetermined.
    ToolResultNotDelivered {
        execution_id: u64,
        id: u64,
        message: String,
    },
    /// Terminal evaluator loss, delivered independently of ordinary events.
    HostLost {
        message: String,
    },
    ToolCall {
        cell_id: u64,
        id: u64,
        name: String,
        input: Value,
    },
    Notify {
        cell_id: u64,
        text: String,
    },
    Content {
        cell_id: u64,
        content: Value,
    },
    Yielded {
        cell_id: u64,
    },
    Done {
        cell_id: u64,
        stored: HashMap<String, Value>,
    },
    Error {
        cell_id: u64,
        message: String,
        stored: HashMap<String, Value>,
    },
    /// Sidecar self resource report (shutdown only).
    ResourceReport {
        user_cpu_ns: u64,
        system_cpu_ns: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        peak_rss_bytes: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rss_bytes: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        threads: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fds: Option<u64>,
    },
}

impl From<RuntimeEvent> for HostOp {
    fn from(event: RuntimeEvent) -> Self {
        match event {
            RuntimeEvent::ToolCall {
                cell_id,
                id,
                name,
                input,
            } => Self::ToolCall {
                cell_id,
                id,
                name,
                input,
            },
            RuntimeEvent::Notify { cell_id, text } => Self::Notify { cell_id, text },
            RuntimeEvent::Content { cell_id, content } => Self::Content { cell_id, content },
            RuntimeEvent::Yielded { cell_id } => Self::Yielded { cell_id },
            RuntimeEvent::Done { cell_id, stored } => Self::Done { cell_id, stored },
            RuntimeEvent::Error {
                cell_id,
                message,
                stored,
            } => Self::Error {
                cell_id,
                message,
                stored,
            },
        }
    }
}

impl From<HostOp> for RuntimeEvent {
    fn from(op: HostOp) -> Self {
        match op {
            HostOp::ToolCall {
                cell_id,
                id,
                name,
                input,
            } => Self::ToolCall {
                cell_id,
                id,
                name,
                input,
            },
            HostOp::Notify { cell_id, text } => Self::Notify { cell_id, text },
            HostOp::Content { cell_id, content } => Self::Content { cell_id, content },
            HostOp::Yielded { cell_id } => Self::Yielded { cell_id },
            HostOp::Done { cell_id, stored } => Self::Done { cell_id, stored },
            HostOp::Error {
                cell_id,
                message,
                stored,
            } => Self::Error {
                cell_id,
                message,
                stored,
            },
            HostOp::ToolResultAccepted { .. }
            | HostOp::ToolResultRefused { .. }
            | HostOp::ToolResultNotDelivered { .. } => Self::Error {
                cell_id: 0,
                message: "result acknowledgement is not a runtime event".into(),
                stored: HashMap::new(),
            },
            HostOp::HostLost { message } => Self::Error {
                cell_id: 0,
                message,
                stored: HashMap::new(),
            },
            HostOp::ResourceReport { .. } => Self::Error {
                cell_id: 0,
                message: "resource report is not a runtime event".into(),
                stored: HashMap::new(),
            },
        }
    }
}
