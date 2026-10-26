//! Compiled MCP client surface.

pub use plakous_mcp::{
    FrozenMcpListing, HeaderName, HeaderValue, MAX_DESCRIPTION_BYTES, MAX_EXPOSED_NAME_BYTES,
    MAX_INLINE_TEXT_BYTES, MAX_LISTED_TOOLS, MAX_LISTING_BYTES, MAX_PROMPT_TEXT_BYTES,
    MAX_SSE_EVENT_BYTES, McpDeviceLogin, McpError, McpErrorKind, McpHttpEndpoint, McpHttpSecret,
    McpHttpSecretResolver, McpLease, McpLocalLogin, McpLocalRetirement, McpLogin, McpLoginOptions,
    McpLoginRemedy, McpLogoutOutcome, McpOAuthSecretResolver, McpOAuthSettings, McpOAuthStatus,
    McpPasteLogin, McpPool, McpPrompt, McpPromptArgument, McpPromptExpansion, McpRemoteLogin,
    McpResource, McpResourceContents, McpResourceTemplate, McpSecretResolver, McpServerSpec,
    McpServerState, McpServerStatus, McpToolPack, McpToolRecord, McpToolSelection, SchemaRefusal,
    SensitiveBytes, exposed_tool_name, prompt_header, untrusted_header,
};
pub use plakous_tools::ServiceSecretValue;
