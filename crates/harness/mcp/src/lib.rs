//! Model Context Protocol client for Plakous.
//!
//! A process-wide [`McpPool`] starts each configured server lazily (a stdio
//! server through the execution environment's service-process seam, a
//! streamable HTTP server ([`McpHttpEndpoint`]) over `plakous-http`), shares it among
//! every session bound to the same definition and workspace root, and stops
//! it after an idle period. At bind a [`McpLease`] freezes the template's
//! selection of the server's tools ([`FrozenMcpListing`]): each input schema
//! is lowered onto the portable schema ([`lower_input_schema`]) and each name
//! is exposed as `mcp__<server>__<tool>` ([`exposed_tool_name`]).
//! [`McpToolPack`] registers the frozen tools as serial, mutating tools owned
//! by their server: a call that may have acted settles as outcome-unknown and
//! is never replayed. A lease also lists and reads the server's resources
//! ([`McpResourceContents`]) and lists and expands its prompts
//! ([`McpPromptExpansion`]), which only a user invokes.
//!
//! Configuration parsing, secret storage and operator views belong to the
//! embedding product.

#![forbid(unsafe_code)]

mod client;
mod error;
mod http;
mod listing;
mod lowering;
mod naming;
mod oauth;
mod pack;
mod pool;
mod prompts;
mod resources;
mod result;
mod spec;

pub use error::{McpError, McpErrorKind};
pub use http::{MAX_SSE_EVENT_BYTES, McpHttpEndpoint, McpHttpSecret, McpHttpSecretResolver};
pub use listing::{
    FrozenMcpListing, MAX_DESCRIPTION_BYTES, MAX_LISTED_TOOLS, MAX_LISTING_BYTES, McpToolRecord,
    McpToolSelection,
};
pub use lowering::{SchemaRefusal, lower_input_schema};
pub use naming::{MAX_EXPOSED_NAME_BYTES, exposed_tool_name};
pub use oauth::{
    McpDeviceLogin, McpLocalLogin, McpLocalRetirement, McpLogin, McpLoginOptions, McpLogoutOutcome,
    McpOAuthSecretResolver, McpOAuthSettings, McpOAuthStatus, McpPasteLogin, McpRemoteLogin,
};
pub use pack::McpToolPack;
pub use plakous_http::{HeaderName, HeaderValue, SensitiveBytes};
pub use pool::{
    CRASH_WINDOW, IDLE_STOP, MAX_IN_FLIGHT_CALLS, McpLease, McpPool, McpServerState,
    McpServerStatus,
};
pub use prompts::{
    MAX_PROMPT_TEXT_BYTES, McpPrompt, McpPromptArgument, McpPromptExpansion, prompt_header,
};
pub use resources::{McpResource, McpResourceContents, McpResourceTemplate};
pub use result::{MAX_INLINE_TEXT_BYTES, untrusted_header};
pub use spec::{DEFAULT_CALL_TIMEOUT, DEFAULT_STARTUP_TIMEOUT, McpSecretResolver, McpServerSpec};

#[cfg(test)]
mod tests;
