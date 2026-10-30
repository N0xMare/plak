//! Typed MCP client errors.

use std::error::Error;
use std::fmt;

/// Login required by an authorization refusal, without frontend command grammar.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct McpLoginRequirement {
    /// Exact server whose login must be replaced.
    pub server: String,
    /// Validated OAuth scopes, in canonical order.
    pub scopes: Vec<String>,
}

/// Named secret required after a server was admitted, without the secret value
/// or a frontend command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpSecretRequirement {
    /// Server whose configuration names the secret.
    pub server: String,
    /// Secret name only.
    pub name: String,
}

/// Resolver failure that still names the missing secret after it is boxed.
#[derive(Debug)]
pub struct McpNamedSecretMissing {
    requirement: McpSecretRequirement,
    message: String,
}

impl McpNamedSecretMissing {
    /// A missing named secret. `message` is operator text and must not contain
    /// the secret value.
    #[must_use]
    pub fn new(
        server: impl Into<String>,
        name: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            requirement: McpSecretRequirement {
                server: server.into(),
                name: name.into(),
            },
            message: message.into(),
        }
    }

    /// The server and secret name this failure requires.
    #[must_use]
    pub const fn requirement(&self) -> &McpSecretRequirement {
        &self.requirement
    }
}

impl fmt::Display for McpNamedSecretMissing {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for McpNamedSecretMissing {}

/// What part of the MCP client failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum McpErrorKind {
    /// A server definition is invalid.
    Definition,
    /// The server process could not start or finish its handshake in time.
    Startup,
    /// `tools/list` failed or exceeded its bounds.
    Listing,
    /// The template selection cannot bind against the server's listing.
    Selection,
    /// The server crashed twice within the restart window and fails fast.
    Failed,
    /// The pool is shut down.
    Closed,
    /// A resource or prompt request was refused, failed or exceeded a bound.
    Request,
    /// An OAuth login, its stored credential or its discovery failed. A
    /// start or request refused this way names the login remedy itself.
    Authorization,
}

/// One MCP client failure.
///
/// The message is operator text: it names servers and tools but never
/// carries secret values. A server's stderr tail is not in it; read it
/// through [`crate::McpLease::status`].
#[derive(Debug)]
pub struct McpError {
    kind: McpErrorKind,
    message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
    pub(super) login: Option<McpLoginRequirement>,
    secret: Option<Box<McpSecretRequirement>>,
}

impl McpError {
    pub(crate) fn new(kind: McpErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
            login: None,
            secret: None,
        }
    }

    pub(crate) fn with_source(
        kind: McpErrorKind,
        message: impl Into<String>,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        let login = super::source_login_requirement(&source);
        let secret = super::source_secret_requirement(&source).map(Box::new);
        Self {
            kind,
            message: message.into(),
            source: Some(Box::new(source)),
            login,
            secret,
        }
    }

    /// A resolver failure. The boxed error stays the source, and a named-secret
    /// requirement is copied out of it when one is present.
    #[must_use]
    pub fn from_resolver(
        kind: McpErrorKind,
        message: impl Into<String>,
        error: Box<dyn Error + Send + Sync>,
    ) -> Self {
        let login = super::source_login_requirement(error.as_ref());
        let secret = super::source_secret_requirement(error.as_ref()).map(Box::new);
        Self {
            kind,
            message: message.into(),
            source: Some(error),
            login,
            secret,
        }
    }

    /// Returns the failure class.
    #[must_use]
    pub const fn kind(&self) -> McpErrorKind {
        self.kind
    }

    /// Returns the operator message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Exact server and scope facts for an authorization repair.
    #[must_use]
    pub const fn login_requirement(&self) -> Option<&McpLoginRequirement> {
        self.login.as_ref()
    }

    /// Exact server and secret name when a named secret is missing.
    #[must_use]
    pub fn secret_requirement(&self) -> Option<&McpSecretRequirement> {
        self.secret.as_deref()
    }
}

impl fmt::Display for McpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for McpError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_resolver_keeps_a_named_secret_after_the_source_is_dropped() {
        let missing = McpNamedSecretMissing::new("notes", "token", "secret token is not set");
        let error = McpError::from_resolver(
            McpErrorKind::Startup,
            "could not connect; secret token is not set",
            Box::new(missing),
        );
        let requirement = error.secret_requirement();
        assert_eq!(requirement.map(|item| item.server.as_str()), Some("notes"));
        assert_eq!(requirement.map(|item| item.name.as_str()), Some("token"));
        assert!(error.source().is_some());
    }
}
