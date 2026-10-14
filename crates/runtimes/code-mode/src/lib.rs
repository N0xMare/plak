//! Parse/DTO helpers for Code Mode presentation.
//!
//! This published-internal package exposes bounded Code Mode values used to
//! parse `exec` source, render tool descriptions, and truncate results. It is
//! not a process-sidecar evaluator and does not spawn `codex-code-mode-host`.
//!
//! The pinned Codex protocol types remain implementation details of the DTO
//! and description helpers.

#![forbid(unsafe_code)]
#![allow(
    missing_docs,
    reason = "published-internal DTO inventory is documented at the module contract boundary"
)]
#![allow(
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    reason = "published-internal boundary methods preserve concise upstream operation docs"
)]

mod description;
mod output;
mod types;

pub use description::ExecDescription;
pub use description::ParsedExecSource;
pub use description::build_exec_description;
pub use description::build_wait_description;
pub use description::parse_exec_source;
pub use output::truncate_content_items;
pub use types::CellId;
pub use types::CodeModeToolKind;
pub use types::ContentItem;
pub use types::ExecuteRequest;
pub use types::ExecutionLimits;
pub use types::ImageDetail;
pub use types::NestedToolCall;
pub use types::Notification;
pub use types::RuntimeResponse;
pub use types::SidecarError;
pub use types::ToolDefinition;
pub use types::ToolName;
pub use types::ToolNamespaceDescription;
pub use types::WaitOutcome;
pub use types::WaitRequest;
pub use types::{
    MAX_CATALOG_BYTES, MAX_CATALOG_TOOLS, MAX_ID_BYTES, MAX_NESTED_INPUT_BYTES,
    MAX_NESTED_OUTPUT_BYTES, MAX_NOTIFICATION_BYTES, MAX_OUTPUT_TOKENS, MAX_PRODUCT_CATALOG_TOOLS,
    MAX_RUNTIME_RESPONSE_BYTES, MAX_SOURCE_BYTES, MAX_TOOL_DESCRIPTION_BYTES, MAX_TOOL_NAME_BYTES,
    MAX_TOOL_SCHEMA_BYTES, MAX_YIELD_TIME_MS, V1_NESTED_TOOL_NAMES,
};

/// Pinned upstream release tag whose protocol this DTO layer translates.
pub const CODEX_RELEASE: &str = "rust-v0.153.3";

/// Exact upstream commit used by the code-mode protocol dependency.
pub const CODEX_COMMIT: &str = "b1a547b1f73ce86205d9222ac19cff334b3b7a2e";
