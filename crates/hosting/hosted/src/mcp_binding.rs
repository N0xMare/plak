//! Named-secret port and feature-selected MCP bindings.

#[cfg(feature = "mcp")]
mod available;
#[cfg(not(feature = "mcp"))]
#[path = "mcp_binding/unavailable.rs"]
mod available;
pub(crate) use available::{BoundMcpTools, bind_mcp_tools, recorded_mcp_tools};

#[derive(Clone, Default)]
pub(crate) struct McpNodeServices {
    pub runner: Option<plakous::runner::RunnerServices>,
    pub product_home: Option<std::path::PathBuf>,
}
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

/// How a compile reaches the MCP servers its selection names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpBinding {
    /// Start or lease each server: a create, open or spawn.
    Lease,
    /// Rebuild the tools from the session's frozen records with no server
    /// behind them: an exact-reopen verification, which never starts,
    /// connects or leases a server and so never renews an MCP login.
    Recorded,
}

impl McpBinding {
    /// A verification ([`TopologyHubAttach::Inspect`]) compiles from records;
    /// every other compile leases.
    ///
    /// [`TopologyHubAttach::Inspect`]: crate::topology_runtime::TopologyHubAttach::Inspect
    pub(crate) fn for_attach(attach: crate::topology_runtime::TopologyHubAttach) -> Self {
        if attach == crate::topology_runtime::TopologyHubAttach::Inspect {
            Self::Recorded
        } else {
            Self::Lease
        }
    }
}

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

    /// The value of this machine's runner secret `name` (`plak runner
    /// secret set`), which only a server placed on the runner reads: a
    /// shelf apart from [`Self::read`]'s. `None`, the default, is a product
    /// that stores no runner secrets.
    ///
    /// # Errors
    ///
    /// Returns a one-line reason when the store refuses the read.
    fn read_runner(&self, name: &str) -> Result<Option<NamedSecretValue>, String> {
        let _ = name;
        Ok(None)
    }

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
