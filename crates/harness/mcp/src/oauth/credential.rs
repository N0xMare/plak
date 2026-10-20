//! The stored MCP login: one private credential record per server.
//!
//! The record lives under the profile [`PROFILE_ID`]; its payload (version
//! [`STORED_VERSION`]) binds the canonical server URI, the issuer and the
//! client id, which also derive the record's account binding.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use plakous_auth::{
    AccountBinding, CredentialFence, CredentialGeneration, CredentialRecord, CredentialSlotId,
    CredentialStatus, OfflineCredentialLifecycle, PrivateCredentialStore, PrivateStoreError,
    SecretBytes, SubscriptionIntegrationProfileId,
};
use plakous_model::{Digest, DigestDomain};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize as _;

use super::McpOAuthSettings;
use crate::error::{McpError, McpErrorKind};

/// The private-store profile of MCP logins.
pub(crate) const PROFILE_ID: &str = "plakous.mcp.oauth.compat.2026-07-28.r1";
/// The payload version this build writes and reads.
pub(crate) const STORED_VERSION: u32 = 1;
const SLOT_ID: &str = "mcp-oauth";
const MAX_FIELD_BYTES: usize = 16 * 1024;

/// How the token endpoint authenticates the client.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TokenEndpointAuth {
    /// A public client: `client_id` in the form only.
    None,
    /// HTTP Basic with the client id and secret.
    ClientSecretBasic,
    /// The client secret in the form.
    ClientSecretPost,
}

/// The payload of one stored login.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredLogin {
    pub(crate) version: u32,
    /// The canonical server URI the login is bound to.
    pub(crate) resource: String,
    /// The `resource=` value the protected-resource metadata confirmed.
    pub(crate) resource_indicator: String,
    pub(crate) issuer: String,
    pub(crate) client_id: String,
    /// A dynamic registration's secret; a pre-registered client's secret is
    /// resolved from the definition at each use and never stored.
    pub(crate) client_secret: Option<String>,
    pub(crate) token_endpoint_auth: TokenEndpointAuth,
    pub(crate) access_token: String,
    /// Unix seconds; `None` when the server gave no lifetime.
    pub(crate) expires_at: Option<u64>,
    pub(crate) refresh_token: Option<String>,
    /// The scope the login requested.
    pub(crate) scope: Option<String>,
    pub(crate) token_endpoint: String,
    pub(crate) revocation_endpoint: Option<String>,
}

impl Drop for StoredLogin {
    fn drop(&mut self) {
        self.access_token.zeroize();
        if let Some(token) = &mut self.refresh_token {
            token.zeroize();
        }
        if let Some(secret) = &mut self.client_secret {
            secret.zeroize();
        }
    }
}

/// Only the version, read when the current shape does not parse.
#[derive(Deserialize)]
struct StoredVersion {
    version: u32,
}

impl StoredLogin {
    fn validate(&self) -> Result<(), McpError> {
        let bounded = |value: &str| !value.is_empty() && value.len() <= MAX_FIELD_BYTES;
        if self.version != STORED_VERSION
            || !bounded(&self.resource)
            || !bounded(&self.resource_indicator)
            || !bounded(&self.issuer)
            || !bounded(&self.client_id)
            || !bounded(&self.access_token)
            || !bounded(&self.token_endpoint)
            || self
                .refresh_token
                .as_deref()
                .is_some_and(|token| !bounded(token))
            || self
                .client_secret
                .as_deref()
                .is_some_and(|secret| !bounded(secret))
        {
            return Err(invalid_stored());
        }
        Ok(())
    }

    /// Whether the login belongs to the endpoint `expected` describes.
    pub(crate) fn belongs_to(&self, expected: &LoginExpectation<'_>) -> bool {
        self.resource == expected.resource
            && expected.issuer.is_none_or(|issuer| issuer == self.issuer)
            && expected
                .client_id
                .is_none_or(|client| client == self.client_id)
    }

    /// Seconds until the access token expires, `None` without a lifetime.
    pub(crate) fn expires_in(&self, now: u64) -> Option<u64> {
        self.expires_at.map(|at| at.saturating_sub(now))
    }

    /// The credential record of this payload at `generation`.
    pub(crate) fn record(
        &self,
        generation: CredentialGeneration,
    ) -> Result<CredentialRecord, McpError> {
        self.validate()?;
        let payload = serde_json::to_vec(self).map_err(|_| invalid_stored())?;
        CredentialRecord::new(
            profile_id()?,
            binding(&self.resource, &self.issuer, &self.client_id)?,
            generation,
            CredentialStatus::Ready,
            SecretBytes::new(payload),
        )
        .map_err(|error| store_failed("mcp login record is invalid", error))
    }

    /// Decodes and validates a payload against its record's binding.
    pub(crate) fn decode(payload: &[u8], fence: &CredentialFence) -> Result<Self, McpError> {
        let stored: Self =
            serde_json::from_slice(payload).map_err(|_| {
                match serde_json::from_slice::<StoredVersion>(payload) {
                    Ok(version) if version.version > STORED_VERSION => newer_build(),
                    _ => invalid_stored(),
                }
            })?;
        stored.validate()?;
        if binding(&stored.resource, &stored.issuer, &stored.client_id)?.as_str()
            != fence.account_binding().as_str()
        {
            return Err(invalid_stored());
        }
        Ok(stored)
    }
}

/// What an endpoint's definition requires of its stored login.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LoginExpectation<'a> {
    /// The canonical server URI.
    pub(crate) resource: &'a str,
    /// The pinned issuer, when the definition pins one.
    pub(crate) issuer: Option<&'a str>,
    /// The pre-registered client id, when the definition names one.
    pub(crate) client_id: Option<&'a str>,
}

impl<'a> LoginExpectation<'a> {
    pub(crate) fn of(settings: &'a McpOAuthSettings, resource: &'a str) -> Self {
        Self {
            resource,
            issuer: settings.issuer(),
            client_id: settings.client().map(|client| client.id.as_str()),
        }
    }
}

/// An MCP server's login as an operator view shows it, read without
/// contacting the server.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum McpOAuthStatus {
    /// No login is stored, or the last one was logged out.
    NotLoggedIn,
    /// A usable login.
    LoggedIn {
        /// Time left on the access token, when the server gave a lifetime.
        expires_in: Option<Duration>,
        /// Whether a refresh token can renew it.
        refreshable: bool,
        /// The scope the login requested.
        scope: Option<String>,
    },
    /// A process is refreshing it now.
    Refreshing,
    /// A refresh outcome is unknown; a new login is required.
    Quarantined,
    /// The stored login cannot be used; a new login is required.
    LoginRequired,
    /// The login was made for another URL, issuer or client.
    OtherServer,
    /// A newer build wrote the login; this build neither uses nor replaces
    /// it without a new login.
    WrittenByNewerBuild,
}

/// Reads the login at `settings`' store for the endpoint at `resource`.
pub(crate) fn status(
    settings: &McpOAuthSettings,
    resource: &str,
) -> Result<McpOAuthStatus, McpError> {
    let Some(store) = open_store(settings.store())? else {
        return Ok(McpOAuthStatus::NotLoggedIn);
    };
    let lifecycle = match store.offline_lifecycle() {
        Ok(lifecycle) => lifecycle,
        Err(PrivateStoreError::WrittenByNewerBuild | PrivateStoreError::ForeignProfile) => {
            return Ok(McpOAuthStatus::WrittenByNewerBuild);
        }
        Err(error) => return Err(store_failed("mcp login could not be read", error)),
    };
    Ok(match lifecycle {
        OfflineCredentialLifecycle::SignedOut | OfflineCredentialLifecycle::Revoked => {
            McpOAuthStatus::NotLoggedIn
        }
        OfflineCredentialLifecycle::Refreshing => McpOAuthStatus::Refreshing,
        OfflineCredentialLifecycle::Quarantined => McpOAuthStatus::Quarantined,
        OfflineCredentialLifecycle::WrittenByNewerBuild => McpOAuthStatus::WrittenByNewerBuild,
        OfflineCredentialLifecycle::Ready => {
            let newer = store
                .inspect_payload(|payload| {
                    serde_json::from_slice::<StoredVersion>(payload.expose_secret())
                        .is_ok_and(|stored| stored.version > STORED_VERSION)
                })
                .map_err(|error| store_failed("mcp login could not be read", error))?;
            if newer == Some(true) {
                return Ok(McpOAuthStatus::WrittenByNewerBuild);
            }
            let Some(fence) = store
                .load()
                .map_err(|error| store_failed("mcp login could not be read", error))?
            else {
                return Ok(McpOAuthStatus::NotLoggedIn);
            };
            match read_ready(&store, &fence) {
                Ok(stored) if stored.belongs_to(&LoginExpectation::of(settings, resource)) => {
                    McpOAuthStatus::LoggedIn {
                        expires_in: stored.expires_in(unix_now()).map(Duration::from_secs),
                        refreshable: stored.refresh_token.is_some(),
                        scope: stored.scope.clone(),
                    }
                }
                Ok(_) => McpOAuthStatus::OtherServer,
                Err(_) => McpOAuthStatus::LoginRequired,
            }
        }
        _ => McpOAuthStatus::LoginRequired,
    })
}

/// Reads and validates the Ready payload at `fence`.
pub(crate) fn read_ready(
    store: &PrivateCredentialStore,
    fence: &CredentialFence,
) -> Result<StoredLogin, McpError> {
    let lease = store
        .acquire_ready(fence)
        .map_err(|error| store_failed("mcp login could not be read", error))?;
    StoredLogin::decode(lease.payload().expose_secret(), fence)
}

/// Opens the record at `path`, or `None` when its directory does not exist
/// (no login was ever made).
pub(crate) fn open_store(path: &Path) -> Result<Option<PrivateCredentialStore>, McpError> {
    if path.parent().is_none_or(|parent| !parent.exists()) {
        return Ok(None);
    }
    let slot = CredentialSlotId::new(SLOT_ID)
        .map_err(|error| store_failed("mcp login slot is invalid", error))?;
    PrivateCredentialStore::open_slot(path, profile_id()?, slot)
        .map(Some)
        .map_err(|error| store_failed("mcp login store could not be opened", error))
}

/// The account binding of a login: a digest of its canonical server URI,
/// issuer and client id.
pub(crate) fn binding(
    resource: &str,
    issuer: &str,
    client_id: &str,
) -> Result<AccountBinding, McpError> {
    let digest = Digest::sha256(
        DigestDomain::ProviderAuthorityBinding,
        1,
        &[
            b"plakous-mcp-oauth-login",
            resource.as_bytes(),
            issuer.as_bytes(),
            client_id.as_bytes(),
        ],
    )
    .map_err(|error| store_failed("mcp login binding failed", error))?;
    AccountBinding::new(format!("mcp-{digest}"))
        .map_err(|error| store_failed("mcp login binding failed", error))
}

fn profile_id() -> Result<SubscriptionIntegrationProfileId, McpError> {
    SubscriptionIntegrationProfileId::new(PROFILE_ID)
        .map_err(|error| store_failed("mcp login profile is invalid", error))
}

/// Unix seconds now; zero before the epoch.
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

pub(crate) fn store_failed(
    message: &str,
    error: impl std::error::Error + Send + Sync + 'static,
) -> McpError {
    McpError::with_source(
        McpErrorKind::Authorization,
        format!("{message}: {error}"),
        error,
    )
}

fn invalid_stored() -> McpError {
    McpError::new(
        McpErrorKind::Authorization,
        "the stored mcp login is invalid",
    )
}

fn newer_build() -> McpError {
    McpError::new(
        McpErrorKind::Authorization,
        "the stored mcp login was written by a newer build",
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn login(resource: &str) -> StoredLogin {
        StoredLogin {
            version: STORED_VERSION,
            resource: resource.to_owned(),
            resource_indicator: resource.to_owned(),
            issuer: "https://auth.example.com".to_owned(),
            client_id: "client-1".to_owned(),
            client_secret: None,
            token_endpoint_auth: TokenEndpointAuth::None,
            access_token: "access-1".to_owned(),
            expires_at: Some(unix_now() + 3600),
            refresh_token: Some("refresh-1".to_owned()),
            scope: Some("files:read".to_owned()),
            token_endpoint: "https://auth.example.com/token".to_owned(),
            revocation_endpoint: None,
        }
    }

    fn settings(directory: &Path) -> McpOAuthSettings {
        McpOAuthSettings::new(directory.join("fake").join("credential.json"))
    }

    #[test]
    fn status_reads_the_login_without_contacting_anything() -> TestResult {
        let home = tempfile::tempdir()?;
        let resource = "https://mcp.example.com/mcp";
        let settings = settings(home.path());
        assert_eq!(status(&settings, resource)?, McpOAuthStatus::NotLoggedIn);
        let directory = home.path().join("fake");
        std::fs::create_dir(&directory)?;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        let store = open_store(settings.store())?.ok_or("store")?;
        store.initialize_selected(&login(resource).record(CredentialGeneration::new(1)?)?)?;
        let McpOAuthStatus::LoggedIn {
            expires_in,
            refreshable,
            scope,
        } = status(&settings, resource)?
        else {
            return Err("not logged in".into());
        };
        assert!(expires_in.is_some_and(|left| left > Duration::from_secs(3500)));
        assert!(refreshable);
        assert_eq!(scope.as_deref(), Some("files:read"));
        // Another URL, issuer or client reads the login as another server's.
        assert_eq!(
            status(&settings, "https://mcp.example.com/other")?,
            McpOAuthStatus::OtherServer
        );
        let pinned = settings.clone().with_issuer("https://other.example.com")?;
        assert_eq!(status(&pinned, resource)?, McpOAuthStatus::OtherServer);
        let client = settings
            .clone()
            .with_issuer("https://auth.example.com")?
            .with_client("client-2", None)?;
        assert_eq!(status(&client, resource)?, McpOAuthStatus::OtherServer);
        let fence = store.load()?.ok_or("fence")?;
        store.logout(fence.generation())?;
        assert_eq!(status(&settings, resource)?, McpOAuthStatus::NotLoggedIn);
        Ok(())
    }

    #[test]
    fn a_payload_must_match_its_binding_and_a_newer_version_is_recognized() -> TestResult {
        let resource = "https://mcp.example.com/mcp";
        let record = login(resource).record(CredentialGeneration::new(1)?)?;
        let fence = record.fence();
        let encoded = serde_json::to_vec(&login(resource))?;
        assert!(StoredLogin::decode(&encoded, &fence).is_ok());
        let other = serde_json::to_vec(&login("https://mcp.example.com/other"))?;
        assert!(StoredLogin::decode(&other, &fence).is_err());
        let newer = br#"{"version":2,"future":true}"#;
        assert_eq!(
            StoredLogin::decode(newer, &fence)
                .err()
                .map(|error| error.to_string()),
            Some(newer_build().to_string())
        );
        let debug = format!("{record:?}");
        assert!(!debug.contains("access-1"));
        Ok(())
    }
}
