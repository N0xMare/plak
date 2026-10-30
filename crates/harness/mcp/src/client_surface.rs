//! Compiled MCP client and its neutral record vocabulary.

#[path = "client.rs"]
mod client;
#[path = "error.rs"]
mod error;
#[path = "http.rs"]
mod http;
mod listing;
#[path = "lowering.rs"]
mod lowering;
#[path = "naming.rs"]
mod naming;
#[path = "oauth.rs"]
mod oauth;
#[path = "pack.rs"]
mod pack;
#[path = "pool.rs"]
mod pool;
#[path = "prompts.rs"]
mod prompts;
#[path = "record.rs"]
mod record;
#[path = "resources.rs"]
mod resources;
#[path = "result.rs"]
mod result;
#[path = "spec.rs"]
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
    McpDeviceLogin, McpLocalLogin, McpLocalRetirement, McpLogin, McpLoginOptions, McpLoginRemedy,
    McpLogoutOutcome, McpOAuthSecretResolver, McpOAuthSettings, McpOAuthStatus, McpPasteLogin,
    McpRemoteLogin,
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
#[path = "tests.rs"]
mod tests;
