//! rquickjs Code Mode protocol and sidecar client.
//!
//! Nested tools surface as [`RuntimeEvent::ToolCall`] for the private driver.
//! The optional in-process engine and worker live in `plakous-js-runtime`.

use std::collections::HashMap;

use serde_json::Value;

mod process_resources;
mod protocol;
mod sidecar;

pub use process_resources::{
    ChildReapBounds, ChildReapEscalation, ProcessResourceUsage, children_rusage_delta,
    reap_child_pid_bounded, sample_children_rusage, sample_self_rusage,
};
pub use protocol::{ClientOp, DEFAULT_YIELD_TIME_MS, HostOp};
pub use sidecar::{
    COMMAND_QUEUE_ITEMS, ChildProcessUsage, NestedDeliveryOutcome, SIDECAR_SHUTDOWN_SETTLE_TIMEOUT,
    SidecarHost, encode_frame,
};

/// Events emitted by one cell on the evaluator thread.
#[derive(Debug)]
pub enum RuntimeEvent {
    /// Nested tool request. Answer through the active evaluator host.
    ToolCall {
        /// Cell execution id.
        cell_id: u64,
        /// Tool call id local to the cell.
        id: u64,
        /// Nested tool name.
        name: String,
        /// JSON arguments.
        input: Value,
    },
    /// `notify(...)` from the cell.
    Notify {
        /// Cell execution id.
        cell_id: u64,
        /// Notification text.
        text: String,
    },
    /// `text` / `image` / `audio` helper output.
    Content {
        /// Cell execution id.
        cell_id: u64,
        /// Helper payload.
        content: Value,
    },
    /// `yield_control()` or cooperative yield timeout.
    Yielded {
        /// Cell execution id.
        cell_id: u64,
    },
    /// Cell completed.
    Done {
        /// Cell execution id.
        cell_id: u64,
        /// `store` writes in this cell.
        stored: HashMap<String, Value>,
    },
    /// Cell failed.
    Error {
        /// Cell execution id.
        cell_id: u64,
        /// Error text.
        message: String,
        /// `store` writes before failure.
        stored: HashMap<String, Value>,
    },
}

/// Engine identity for the rquickjs sidecar host.
pub const SIDECAR_DEPLOYMENT: &str = "rquickjs-host";
/// Bootstrap + helper contract revision.
pub const BOOTSTRAP_REVISION: &str = "plakous-js-eval-bootstrap.v5";
