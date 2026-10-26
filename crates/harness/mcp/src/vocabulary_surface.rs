//! Frozen MCP records remain readable without a client or transport.

#[path = "error.rs"]
mod error;
#[path = "naming.rs"]
mod naming;
#[path = "record.rs"]
mod record;

pub use error::{McpError, McpErrorKind};
pub use naming::{MAX_EXPOSED_NAME_BYTES, exposed_tool_name};
pub use record::{MAX_DESCRIPTION_BYTES, McpToolRecord, McpToolSelection};
