//! MCP authorization (specification 2026-07-28) for streamable HTTP servers.
//!
//! An endpoint with [`McpOAuthSettings`] authenticates with tokens from one
//! private credential record. Logins store that record; the transport reads
//! and refreshes it. Nothing here starts a login on its own.

mod credential;
mod exchange;
mod login;
mod session;

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

pub use credential::McpOAuthStatus;
pub(crate) use credential::status;
pub(crate) use exchange::OAuthHttp;
pub use login::{
    McpDeviceLogin, McpLocalLogin, McpLocalRetirement, McpLogin, McpLoginOptions, McpLogoutOutcome,
    McpPasteLogin, McpRemoteLogin,
};
pub(crate) use session::{Bearer, OAuthSession};

use crate::error::{McpError, McpErrorKind};

/// Resolves a pre-registered client's secret at each use.
pub type McpOAuthSecretResolver = Arc<
    dyn Fn() -> Result<plakous_http::SensitiveBytes, Box<dyn std::error::Error + Send + Sync>>
        + Send
        + Sync,
>;

/// Names the login that fixes a credential failure, with the scope a
/// step-up needs: operator text such as `run plak mcp login docs`.
pub type McpLoginRemedy = Arc<dyn Fn(Option<&str>) -> String + Send + Sync>;

/// One HTTP endpoint's OAuth settings: where its login is stored, which
/// authorization server and client it uses, and its login callback port.
///
/// The definition digest covers the issuer pin, the client id, whether a
/// client secret is configured and the callback port; never the store
/// location, a secret or a token.
#[derive(Clone)]
pub struct McpOAuthSettings {
    store: PathBuf,
    issuer: Option<String>,
    client: Option<PreRegisteredClient>,
    callback_port: Option<u16>,
    login_remedy: Option<McpLoginRemedy>,
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
            callback_port: None,
            login_remedy: None,
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

    /// Fixes the loopback callback port of a login, for a client registered
    /// with exactly `http://127.0.0.1:<port>/callback`.
    #[must_use]
    pub const fn with_callback_port(mut self, port: u16) -> Self {
        self.callback_port = Some(port);
        self
    }

    /// Sets the text credential failures name as their remedy; without it
    /// they say to log in to the server again.
    #[must_use]
    pub fn with_login_remedy(mut self, remedy: McpLoginRemedy) -> Self {
        self.login_remedy = Some(remedy);
        self
    }

    pub(crate) fn login_remedy(&self, scope: Option<&str>) -> String {
        let scope = scope.filter(|scope| plakous_auth::oauth_workflow::valid_scope_list(scope));
        match (&self.login_remedy, scope) {
            (Some(remedy), scope) => remedy(scope),
            (None, Some(scope)) => format!("log in to it again with scope \"{scope}\""),
            (None, None) => "log in to it again".to_owned(),
        }
    }

    pub(crate) const fn callback_port(&self) -> Option<u16> {
        self.callback_port
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
        parts.push(self.callback_port.unwrap_or(0).to_be_bytes().to_vec());
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
            .field("callback_port", &self.callback_port)
            .finish_non_exhaustive()
    }
}
