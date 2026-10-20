//! Typed MCP client errors.

use std::error::Error;
use std::fmt;

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
}

impl McpError {
    pub(crate) fn new(kind: McpErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    pub(crate) fn with_source(
        kind: McpErrorKind,
        message: impl Into<String>,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            source: Some(Box::new(source)),
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
