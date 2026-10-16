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

/// One frozen stdio MCP server definition.
///
/// Built by the embedding host from its configuration; nothing a model
/// supplies reaches it. Two definitions with the same digest share one pool
/// entry per workspace root.
#[derive(Clone)]
pub struct McpServerSpec {
    name: McpServerName,
    program: Arc<str>,
    arguments: Vec<String>,
    environment: Vec<(EnvironmentVariableName, String)>,
    secret_names: Vec<EnvironmentVariableName>,
    secrets: Option<McpSecretResolver>,
    startup_timeout: Duration,
    call_timeout: Duration,
    allowlist: Option<Vec<String>>,
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
        Self {
            name,
            program: program.into(),
            arguments: arguments.into_iter().collect(),
            environment: Vec::new(),
            secret_names: Vec::new(),
            secrets: None,
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            call_timeout: DEFAULT_CALL_TIMEOUT,
            allowlist: None,
        }
    }

    /// Declares literal environment values for the server process.
    #[must_use]
    pub fn with_environment(
        mut self,
        environment: impl IntoIterator<Item = (EnvironmentVariableName, String)>,
    ) -> Self {
        self.environment = environment.into_iter().collect();
        self
    }

    /// Declares the secret environment names and how to resolve their values.
    ///
    /// The names are part of the definition digest; the values are not.
    #[must_use]
    pub fn with_secret_environment(
        mut self,
        names: impl IntoIterator<Item = EnvironmentVariableName>,
        resolver: McpSecretResolver,
    ) -> Self {
        self.secret_names = names.into_iter().collect();
        self.secrets = Some(resolver);
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
        let startup = self.startup_timeout.as_millis().to_be_bytes();
        let call = self.call_timeout.as_millis().to_be_bytes();
        let mut parts: Vec<&[u8]> = vec![
            b"plakous-mcp/server-definition",
            b"stdio",
            self.name.as_str().as_bytes(),
            self.program.as_bytes(),
            &startup,
            &call,
        ];
        let argument_count = self.arguments.len().to_be_bytes();
        parts.push(&argument_count);
        parts.extend(self.arguments.iter().map(String::as_bytes));
        let environment_count = self.environment.len().to_be_bytes();
        parts.push(&environment_count);
        for (name, value) in &self.environment {
            parts.push(name.as_str().as_bytes());
            parts.push(value.as_bytes());
        }
        let secret_count = self.secret_names.len().to_be_bytes();
        parts.push(&secret_count);
        parts.extend(
            self.secret_names
                .iter()
                .map(|name| name.as_str().as_bytes()),
        );
        let allowlist_count = self
            .allowlist
            .as_ref()
            .map_or(u64::MAX, |names| names.len() as u64)
            .to_be_bytes();
        parts.push(&allowlist_count);
        parts.extend(self.allowlist.iter().flatten().map(String::as_bytes));
        Digest::sha256(DigestDomain::ToolDefinition, 1, &parts).map_err(|error| {
            McpError::with_source(
                McpErrorKind::Definition,
                format!("mcp server {} definition digest failed", self.name),
                error,
            )
        })
    }

    /// Builds one spawn request, resolving secrets now.
    pub(crate) fn service_request(&self) -> Result<ServiceProcessRequest, McpError> {
        let secrets = match &self.secrets {
            Some(resolve) => resolve().map_err(|error| {
                McpError::new(
                    McpErrorKind::Definition,
                    format!(
                        "mcp server {} secrets could not be resolved: {error}",
                        self.name
                    ),
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
                    "mcp server {} secret resolution did not return exactly its declared names",
                    self.name
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
                format!("mcp server {} process definition is invalid", self.name),
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
            .field("argument_count", &self.arguments.len())
            .field("environment_count", &self.environment.len())
            .field("secret_names", &self.secret_names)
            .field("startup_timeout", &self.startup_timeout)
            .field("call_timeout", &self.call_timeout)
            .field("allowlist", &self.allowlist)
            .finish_non_exhaustive()
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
        assert_eq!(
            spec.service_request().err().map(|error| error.kind()),
            Some(McpErrorKind::Definition)
        );
        Ok(())
    }
}
