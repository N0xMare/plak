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

pub use error::{
    McpError, McpErrorKind, McpLoginRequirement, McpNamedSecretMissing, McpSecretRequirement,
};

fn source_login_requirement(
    source: &(dyn std::error::Error + 'static),
) -> Option<McpLoginRequirement> {
    match http::classify(source) {
        Some(http::HttpRefusal::Authorization { text, .. }) => text.login,
        _ => source
            .downcast_ref::<McpError>()
            .and_then(McpError::login_requirement)
            .cloned(),
    }
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

impl McpError {
    pub(crate) fn with_login(mut self, login: Option<McpLoginRequirement>) -> Self {
        self.login = login;
        self
    }
}
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
#[path = "tests.rs"]
mod tests;
