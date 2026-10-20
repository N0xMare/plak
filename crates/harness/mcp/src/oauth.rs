//! MCP authorization (specification 2026-07-28) for streamable HTTP servers.
//!
//! An endpoint with [`McpOAuthSettings`] authenticates with tokens from one
//! private credential record. Logins store that record; the transport reads
//! and refreshes it. Nothing here starts a login on its own.

mod credential;

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

pub use credential::McpOAuthStatus;
pub(crate) use credential::status;

use crate::error::{McpError, McpErrorKind};

/// Resolves a pre-registered client's secret at each use.
pub type McpOAuthSecretResolver = Arc<
    dyn Fn() -> Result<plakous_http::SensitiveBytes, Box<dyn std::error::Error + Send + Sync>>
        + Send
        + Sync,
>;

/// One HTTP endpoint's OAuth settings: where its login is stored and which
/// authorization server and client it uses.
///
/// The definition digest covers the issuer pin, the client id and whether a
/// client secret is configured; never the store location, a secret or a
/// token.
#[derive(Clone)]
pub struct McpOAuthSettings {
    store: PathBuf,
    issuer: Option<String>,
    client: Option<PreRegisteredClient>,
}

#[derive(Clone)]
pub(crate) struct PreRegisteredClient {
    pub(crate) id: String,
    pub(crate) secret: Option<McpOAuthSecretResolver>,
}

impl McpOAuthSettings {
    /// Settings whose login is the private credential record at `store`;
    /// its directory is created `0700` by the embedder before a login.
    #[must_use]
    pub fn new(store: impl Into<PathBuf>) -> Self {
        Self {
            store: store.into(),
            issuer: None,
            client: None,
        }
    }

    /// Pins the authorization server: discovery must list `issuer`, and it
    /// is used whatever else is listed.
    ///
    /// # Errors
    ///
    /// Refuses an issuer that is not an absolute `https://` or `http://` URL
    /// without query or fragment.
    pub fn with_issuer(mut self, issuer: &str) -> Result<Self, McpError> {
        plakous_auth::oauth_workflow::authorization_server_metadata_urls(issuer).map_err(|_| {
            McpError::new(
                McpErrorKind::Definition,
                "mcp oauth issuer must be an absolute https:// URL without query or fragment",
            )
        })?;
        self.issuer = Some(issuer.to_owned());
        Ok(self)
    }

    /// Uses a client registered with the pinned issuer in advance, with an
    /// optional secret resolved at each use.
    ///
    /// # Errors
    ///
    /// Refuses an id that is empty, over 255 bytes or not visible ASCII.
    pub fn with_client(
        mut self,
        id: impl Into<String>,
        secret: Option<McpOAuthSecretResolver>,
    ) -> Result<Self, McpError> {
        let id = id.into();
        if id.is_empty() || id.len() > 255 || !id.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
        {
            return Err(McpError::new(
                McpErrorKind::Definition,
                "mcp oauth client id must be 1 to 255 bytes of visible ASCII",
            ));
        }
        self.client = Some(PreRegisteredClient { id, secret });
        Ok(self)
    }

    pub(crate) fn store(&self) -> &std::path::Path {
        &self.store
    }

    pub(crate) fn issuer(&self) -> Option<&str> {
        self.issuer.as_deref()
    }

    pub(crate) fn client(&self) -> Option<&PreRegisteredClient> {
        self.client.as_ref()
    }

    /// Refuses a pre-registered client without an issuer pin, so a client
    /// secret only ever reaches the issuer the operator named.
    pub(crate) fn check(&self) -> Result<(), McpError> {
        if self.client.is_some() && self.issuer.is_none() {
            return Err(McpError::new(
                McpErrorKind::Definition,
                "mcp oauth client id requires an issuer pin",
            ));
        }
        Ok(())
    }

    pub(crate) fn digest_parts(&self, parts: &mut Vec<Vec<u8>>) {
        parts.push(b"oauth".to_vec());
        parts.push(self.issuer.as_deref().unwrap_or("").as_bytes().to_vec());
        parts.push(
            self.client
                .as_ref()
                .map_or(&b""[..], |client| client.id.as_bytes())
                .to_vec(),
        );
        parts.push(vec![u8::from(
            self.client
                .as_ref()
                .is_some_and(|client| client.secret.is_some()),
        )]);
    }
}

impl fmt::Debug for McpOAuthSettings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpOAuthSettings")
            .field("issuer", &self.issuer)
            .field(
                "client_id",
                &self.client.as_ref().map(|client| client.id.as_str()),
            )
            .field(
                "client_secret",
                &self
                    .client
                    .as_ref()
                    .is_some_and(|client| client.secret.is_some()),
            )
            .finish_non_exhaustive()
    }
}
