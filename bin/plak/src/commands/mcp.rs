//! MCP command vocabulary remains available without the client.

use crate::product_config::{ConfigWriteScope, McpServerConfig};

#[cfg(feature = "mcp")]
mod available;
#[cfg(not(feature = "mcp"))]
#[path = "mcp/unavailable.rs"]
mod available;
pub(crate) use available::{run, session_overview};

/// One parsed `plak mcp` invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum McpCommand {
    Add {
        id: String,
        scope: ConfigWriteScope,
        server: McpServerConfig,
        replace: bool,
    },
    List,
    Show {
        id: String,
    },
    Test {
        id: String,
    },
    Login {
        id: String,
        scope: Option<String>,
        remote: bool,
    },
    Logout {
        id: String,
        local: bool,
    },
    Prompts {
        id: String,
        prompt: Option<String>,
        arguments: std::collections::BTreeMap<String, String>,
    },
    Remove {
        id: String,
        scope: Option<ConfigWriteScope>,
        force: bool,
    },
    Enable {
        id: String,
        template: String,
        tools: Vec<String>,
    },
    Disable {
        id: String,
        template: String,
    },
}
