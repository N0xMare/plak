//! Frozen MCP records remain readable without a client or transport.

#[path = "error.rs"]
mod error;
#[path = "naming.rs"]
mod naming;
#[path = "record.rs"]
mod record;

pub use error::{
    McpError, McpErrorKind, McpLoginRequirement, McpNamedSecretMissing, McpSecretRequirement,
};

fn source_login_requirement(
    source: &(dyn std::error::Error + 'static),
) -> Option<McpLoginRequirement> {
    source
        .downcast_ref::<McpError>()
        .and_then(McpError::login_requirement)
        .cloned()
}

fn source_secret_requirement(
    source: &(dyn std::error::Error + 'static),
) -> Option<McpSecretRequirement> {
    if let Some(missing) = source.downcast_ref::<McpNamedSecretMissing>() {
        return Some(missing.requirement().clone());
    }
    source
        .downcast_ref::<McpError>()
        .and_then(McpError::secret_requirement)
        .cloned()
}
pub use naming::{MAX_EXPOSED_NAME_BYTES, exposed_tool_name};
pub use record::{MAX_DESCRIPTION_BYTES, McpToolRecord, McpToolSelection};
