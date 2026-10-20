//! Frozen MCP server definitions.

use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use plakous_model::{Digest, DigestDomain};
use plakous_tools::{
    EnvironmentVariableName, MAX_SERVICE_STDERR_TAIL_BYTES, McpServerName, ServiceProcessRequest,
    ServiceSecretValue,
};

use crate::error::{McpError, McpErrorKind};
use crate::http::McpHttpEndpoint;

/// Default bound on spawn, handshake and the first `tools/list`.
pub const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Default bound on one `tools/call`.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(300);

/// Resolves a server's secret environment at each spawn.
///
/// Called again on every restart, so secret values are not retained by the
/// pool between spawns.
pub type McpSecretResolver = Arc<
    dyn Fn() -> Result<
            Vec<(EnvironmentVariableName, ServiceSecretValue)>,
            Box<dyn Error + Send + Sync>,
        > + Send
        + Sync,
>;

/// One frozen MCP server definition: a stdio process or a streamable HTTP
/// endpoint.
///
/// Built by the embedding host from its configuration; nothing a model
/// supplies reaches it. Two definitions with the same digest share one pool
/// entry per workspace root.
#[derive(Clone)]
pub struct McpServerSpec {
    name: McpServerName,
    transport: Transport,
    startup_timeout: Duration,
    call_timeout: Duration,
    allowlist: Option<Vec<String>>,
    resources: bool,
}

/// How a server is reached.
#[derive(Clone)]
pub(crate) enum Transport {
    Stdio(StdioServer),
    Http(McpHttpEndpoint),
}

#[derive(Clone)]
pub(crate) struct StdioServer {
    program: Arc<str>,
    arguments: Vec<String>,
    environment: Vec<(EnvironmentVariableName, String)>,
    secret_names: Vec<EnvironmentVariableName>,
    secrets: Option<McpSecretResolver>,
}

impl McpServerSpec {
    /// A stdio server started as `program arguments...`.
    ///
    /// `program` is absolute or a basename looked up on the child's PATH.
    #[must_use]
    pub fn stdio(
        name: McpServerName,
        program: impl Into<Arc<str>>,
        arguments: impl IntoIterator<Item = String>,
    ) -> Self {
        Self::new(
            name,
            Transport::Stdio(StdioServer {
                program: program.into(),
                arguments: arguments.into_iter().collect(),
                environment: Vec::new(),
                secret_names: Vec::new(),
                secrets: None,
            }),
        )
    }

    /// A streamable HTTP server at `endpoint`.
    #[must_use]
    pub fn http(name: McpServerName, endpoint: McpHttpEndpoint) -> Self {
        Self::new(name, Transport::Http(endpoint))
    }

    const fn new(name: McpServerName, transport: Transport) -> Self {
        Self {
            name,
            transport,
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            call_timeout: DEFAULT_CALL_TIMEOUT,
            allowlist: None,
            resources: false,
        }
    }

    /// Declares literal environment values for a stdio server process; an
    /// HTTP definition has no process and ignores them.
    #[must_use]
    pub fn with_environment(
        mut self,
        environment: impl IntoIterator<Item = (EnvironmentVariableName, String)>,
    ) -> Self {
        if let Transport::Stdio(stdio) = &mut self.transport {
            stdio.environment = environment.into_iter().collect();
        }
        self
    }

    /// Declares a stdio server's secret environment names and how to resolve
    /// their values; an HTTP definition ignores them.
    ///
    /// The names are part of the definition digest; the values are not.
    #[must_use]
    pub fn with_secret_environment(
        mut self,
        names: impl IntoIterator<Item = EnvironmentVariableName>,
        resolver: McpSecretResolver,
    ) -> Self {
        if let Transport::Stdio(stdio) = &mut self.transport {
            stdio.secret_names = names.into_iter().collect();
            stdio.secrets = Some(resolver);
        }
        self
    }

    /// Bounds spawn, handshake and the first `tools/list`.
    #[must_use]
    pub const fn with_startup_timeout(mut self, timeout: Duration) -> Self {
        self.startup_timeout = timeout;
        self
    }

    /// Bounds each `tools/call`; progress does not extend it.
    #[must_use]
    pub const fn with_call_timeout(mut self, timeout: Duration) -> Self {
        self.call_timeout = timeout;
        self
    }

    /// Restricts the MCP tool names any template may select from this server.
    #[must_use]
    pub fn with_allowlist(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.allowlist = Some(names.into_iter().collect());
        self
    }

    /// Enables the server's resources tool, `mcp__<server>__read_resource`,
    /// which every bind then includes; off by default.
    #[must_use]
    pub const fn with_resources(mut self, enabled: bool) -> Self {
        self.resources = enabled;
        self
    }

    /// Returns whether the resources tool is enabled.
    #[must_use]
    pub const fn resources(&self) -> bool {
        self.resources
    }

    /// Returns the configured server id.
    #[must_use]
    pub const fn name(&self) -> &McpServerName {
        &self.name
    }

    /// Returns the call bound.
    #[must_use]
    pub const fn call_timeout(&self) -> Duration {
        self.call_timeout
    }

    /// Returns the startup bound.
    #[must_use]
    pub const fn startup_timeout(&self) -> Duration {
        self.startup_timeout
    }

    /// Reads an OAuth endpoint's stored login without contacting anything;
    /// `None` for a definition without OAuth.
    ///
    /// # Errors
    ///
    /// Returns an authorization error when the store cannot be read.
    pub fn oauth_status(&self) -> Result<Option<crate::McpOAuthStatus>, McpError> {
        let Transport::Http(endpoint) = &self.transport else {
            return Ok(None);
        };
        let Some(settings) = endpoint.oauth() else {
            return Ok(None);
        };
        crate::oauth::status(settings, &endpoint.canonical_resource()?).map(Some)
    }

    pub(crate) const fn transport(&self) -> &Transport {
        &self.transport
    }

    pub(crate) fn allows(&self, mcp_name: &str) -> bool {
        self.allowlist
            .as_ref()
            .is_none_or(|names| names.iter().any(|name| name == mcp_name))
    }

    /// The content identity of everything but secret values.
    ///
    /// # Errors
    ///
    /// Returns a definition error when the digest cannot be framed.
    pub fn definition_digest(&self) -> Result<Digest, McpError> {
        let startup = self.startup_timeout.as_millis().to_be_bytes().to_vec();
        let call = self.call_timeout.as_millis().to_be_bytes().to_vec();
        let mut parts = vec![b"plakous-mcp/server-definition".to_vec()];
        match &self.transport {
            Transport::Stdio(stdio) => {
                parts.extend([
                    b"stdio".to_vec(),
                    self.name.as_str().as_bytes().to_vec(),
                    stdio.program.as_bytes().to_vec(),
                    startup,
                    call,
                ]);
                stdio.digest_parts(&mut parts);
            }
            Transport::Http(endpoint) => {
                parts.extend([
                    b"http".to_vec(),
                    self.name.as_str().as_bytes().to_vec(),
                    startup,
                    call,
                ]);
                endpoint.digest_parts(&mut parts);
            }
        }
        let allowlist_count = self
            .allowlist
            .as_ref()
            .map_or(u64::MAX, |names| names.len() as u64);
        parts.push(allowlist_count.to_be_bytes().to_vec());
        parts.extend(
            self.allowlist
                .iter()
                .flatten()
                .map(|name| name.as_bytes().to_vec()),
        );
        if self.resources {
            parts.push(b"resources".to_vec());
        }
        let parts = parts.iter().map(Vec::as_slice).collect::<Vec<_>>();
        Digest::sha256(DigestDomain::ToolDefinition, 1, &parts).map_err(|error| {
            McpError::with_source(
                McpErrorKind::Definition,
                format!("mcp server {} definition digest failed", self.name),
                error,
            )
        })
    }
}

impl StdioServer {
    fn digest_parts(&self, parts: &mut Vec<Vec<u8>>) {
        parts.push(self.arguments.len().to_be_bytes().to_vec());
        parts.extend(
            self.arguments
                .iter()
                .map(|argument| argument.as_bytes().to_vec()),
        );
        parts.push(self.environment.len().to_be_bytes().to_vec());
        for (name, value) in &self.environment {
            parts.push(name.as_str().as_bytes().to_vec());
            parts.push(value.as_bytes().to_vec());
        }
        parts.push(self.secret_names.len().to_be_bytes().to_vec());
        parts.extend(
            self.secret_names
                .iter()
                .map(|name| name.as_str().as_bytes().to_vec()),
        );
    }

    /// Builds one spawn request, resolving secrets now.
    pub(crate) fn service_request(
        &self,
        server: &McpServerName,
    ) -> Result<ServiceProcessRequest, McpError> {
        let secrets = match &self.secrets {
            Some(resolve) => resolve().map_err(|error| {
                McpError::new(
                    McpErrorKind::Definition,
                    format!("mcp server {server} secrets could not be resolved: {error}"),
                )
            })?,
            None => Vec::new(),
        };
        let resolved = secrets.iter().map(|(name, _)| name).collect::<Vec<_>>();
        if resolved.len() != self.secret_names.len()
            || self
                .secret_names
                .iter()
                .any(|name| !resolved.contains(&name))
        {
            return Err(McpError::new(
                McpErrorKind::Definition,
                format!(
                    "mcp server {server} secret resolution did not return exactly its declared names"
                ),
            ));
        }
        ServiceProcessRequest::new(
            Arc::clone(&self.program),
            self.arguments.iter().cloned(),
            self.environment.iter().cloned(),
            secrets,
            MAX_SERVICE_STDERR_TAIL_BYTES,
        )
        .map_err(|error| {
            McpError::with_source(
                McpErrorKind::Definition,
                format!("mcp server {server} process definition is invalid"),
                error,
            )
        })
    }
}

impl fmt::Debug for McpServerSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpServerSpec")
            .field("name", &self.name)
            .field("transport", &self.transport)
            .field("startup_timeout", &self.startup_timeout)
            .field("call_timeout", &self.call_timeout)
            .field("allowlist", &self.allowlist)
            .field("resources", &self.resources)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for Transport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio(stdio) => formatter
                .debug_struct("Stdio")
                .field("argument_count", &stdio.arguments.len())
                .field("environment_count", &stdio.environment.len())
                .field("secret_names", &stdio.secret_names)
                .finish_non_exhaustive(),
            Self::Http(endpoint) => endpoint.fmt(formatter),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn Error>>;

    #[test]
    fn the_digest_covers_the_definition_but_not_secret_values() -> TestResult {
        let name = McpServerName::new("github")?;
        let token = EnvironmentVariableName::new("GITHUB_TOKEN")?;
        let with_secret = |value: &'static str| -> McpServerSpec {
            McpServerSpec::stdio(name.clone(), "github-mcp", ["stdio".to_owned()])
                .with_secret_environment(
                    [token.clone()],
                    Arc::new(move || {
                        Ok(vec![(
                            EnvironmentVariableName::new("GITHUB_TOKEN")?,
                            ServiceSecretValue::new(value.to_owned())?,
                        )])
                    }),
                )
        };
        let base = with_secret("one").definition_digest()?;
        assert_eq!(base, with_secret("two").definition_digest()?);
        let edits = [
            McpServerSpec::stdio(name.clone(), "github-mcp", ["stdio".to_owned()]),
            with_secret("one").with_allowlist(["create_issue".to_owned()]),
            with_secret("one").with_resources(true),
            with_secret("one").with_call_timeout(Duration::from_secs(1)),
            with_secret("one").with_startup_timeout(Duration::from_secs(1)),
            with_secret("one")
                .with_environment([(EnvironmentVariableName::new("MODE")?, "x".to_owned())]),
            McpServerSpec::stdio(name.clone(), "github-mcp", ["other".to_owned()]),
        ];
        for edited in edits {
            assert_ne!(base, edited.definition_digest()?, "{edited:?}");
        }
        assert!(!format!("{:?}", with_secret("one")).contains("one\""));
        Ok(())
    }

    #[test]
    fn secret_resolution_must_return_exactly_the_declared_names() -> TestResult {
        let spec = McpServerSpec::stdio(McpServerName::new("github")?, "github-mcp", [])
            .with_secret_environment(
                [EnvironmentVariableName::new("GITHUB_TOKEN")?],
                Arc::new(|| Ok(Vec::new())),
            );
        let Transport::Stdio(stdio) = spec.transport() else {
            return Err("not a stdio definition".into());
        };
        assert_eq!(
            stdio
                .service_request(spec.name())
                .err()
                .map(|error| error.kind()),
            Some(McpErrorKind::Definition)
        );
        Ok(())
    }
}
