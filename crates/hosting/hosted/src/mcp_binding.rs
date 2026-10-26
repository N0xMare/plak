//! Named-secret port and feature-selected MCP bindings.

#[cfg(feature = "mcp")]
mod available;
#[cfg(not(feature = "mcp"))]
#[path = "mcp_binding/unavailable.rs"]
mod available;
pub(crate) use available::{BoundMcpTools, bind_mcp_tools};
#[cfg(feature = "mcp")]
pub use available::{
    McpProbe, get_mcp_prompt, list_mcp_prompts, mcp_login_status, mcp_oauth_spec, probe_mcp_server,
};

#[cfg(feature = "mcp")]
mod secret_value;
#[cfg(not(feature = "mcp"))]
#[path = "mcp_binding/secret_value_unavailable.rs"]
mod secret_value;
pub use secret_value::NamedSecretValue;

/// The product's named secrets (`plak secret set <name>`).
///
/// Hosting asks for a value by name at each server spawn and never reads the
/// secret store itself.
pub trait NamedSecrets: Send + Sync + 'static {
    /// The named secret's value, or `None` when it is not stored.
    ///
    /// # Errors
    ///
    /// Returns a one-line reason when the store refuses the read.
    fn read(&self, name: &str) -> Result<Option<NamedSecretValue>, String>;

    /// Where the OAuth login of MCP server `server` is stored: a private
    /// credential record in a directory of its own. `None`, the default, is
    /// a product that stores no MCP logins, where `auth = "oauth"` refuses.
    fn mcp_login_file(&self, server: &str) -> Option<std::path::PathBuf> {
        let _ = server;
        None
    }
}

/// A product with no named-secret store: every secret is unset.
pub(crate) struct NoNamedSecrets;

impl NamedSecrets for NoNamedSecrets {
    fn read(&self, _name: &str) -> Result<Option<NamedSecretValue>, String> {
        Ok(None)
    }
}
