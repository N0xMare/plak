//! Where this process's credentials live, resolved once per process.
//!
//! [`HostCustody`] is read from the home's own `config.toml` (never a
//! project overlay) when a process first needs it; a host resolves it at
//! startup and every route, subscription, console and admin consumer takes
//! that one value. A config that does not read or parse is an error: it
//! never falls back to local custody. Changing `[gateway]` needs a restart.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use plakous::gateway::{GatewayCustody, GatewayEndpoint, SensitiveBytes};

use super::address::GatewayAddress;
use crate::home::paths::PlakPaths;
use crate::product_config::ProductConfig;

/// This process's credential custody.
#[derive(Clone)]
pub(crate) enum HostCustody {
    /// No `[gateway] address`: this process opens the store and key files.
    Local,
    /// Every route and account verb goes to this gateway.
    Gateway(Arc<GatewayLink>),
}

/// A configured gateway: relay custody plus the admin link.
pub(crate) struct GatewayLink {
    admin: AdminLink,
    custody: GatewayCustody,
}

/// Where admin verbs go. This does not connect.
pub(crate) struct AdminLink {
    pub(crate) address: GatewayAddress,
    key: Option<String>,
}

impl AdminLink {
    pub(crate) fn accounts(&self) -> Result<Vec<String>, String> {
        use plakous::gateway::protocol::admin::{AdminRequest, AdminResponse};
        match self.call(&AdminRequest::List {})? {
            AdminResponse::Accounts { accounts } => Ok(accounts
                .into_iter()
                .map(|account| account.as_str().to_owned())
                .collect()),
            AdminResponse::Refused { refusal } => {
                Err(format!("the gateway refused account listing: {refusal:?}"))
            }
            _ => Err("the gateway returned an unexpected account-list response".to_owned()),
        }
    }

    pub(crate) fn call(
        &self,
        request: &impl serde::Serialize,
    ) -> Result<plakous::gateway::protocol::admin::AdminResponse, String> {
        super::client::admin(&self.address, self.key.as_deref(), request)
    }

    pub(crate) fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }

    /// One subscription account's credential posture from the gateway's
    /// admin status, in the shape an offline read has.
    pub(crate) fn credential_status(
        &self,
        account: &str,
    ) -> Result<crate::presentation::CliStatus, String> {
        use crate::presentation::{CompatibilityState, CredentialState};
        use plakous::gateway::protocol::facts::CredentialStatus;
        let facts = self
            .status(account)
            .map_err(|error| format!("gateway: {error}"))?
            .ok_or_else(|| format!("gateway: {account} is not a gateway account"))?;
        let credential = match facts.credential.status {
            CredentialStatus::SignedOut => CredentialState::SignedOut,
            CredentialStatus::Ready => CredentialState::Ready,
            CredentialStatus::RefreshRequired => CredentialState::RefreshRequired,
            CredentialStatus::Refreshing => CredentialState::Refreshing,
            CredentialStatus::Quarantined => CredentialState::Quarantined,
            CredentialStatus::LoginRequired => CredentialState::LoginRequired,
            CredentialStatus::ProfileChanged => CredentialState::ProfileChanged,
            CredentialStatus::WrittenByNewerBuild => CredentialState::WrittenByNewerBuild,
            CredentialStatus::Revoked => CredentialState::Revoked,
        };
        // The gateway's status rows carry no consent; the gateway refuses a
        // dispatch that needs one, so this row reports the credential only.
        Ok(crate::presentation::CliStatus {
            compatibility: CompatibilityState::Current,
            credential,
        })
    }

    /// One account's facts from the gateway's admin status; `None` when the
    /// gateway does not know the account.
    pub(crate) fn status(
        &self,
        account: &str,
    ) -> Result<Option<plakous::gateway::protocol::facts::AccountFacts>, String> {
        use plakous::gateway::protocol::admin::{AdminRequest, AdminResponse};
        let gateway_account = plakous::gateway::protocol::GatewayAccount::new(account.to_owned())
            .map_err(|error| error.to_string())?;
        match self.call(&AdminRequest::Status {
            accounts: vec![gateway_account],
        })? {
            AdminResponse::Status { rows } => Ok(rows
                .into_iter()
                .map(|row| row.facts)
                .find(|facts| facts.account.as_str() == account)),
            AdminResponse::Refused { refusal } => {
                Err(format!("the gateway refused status: {refusal:?}"))
            }
            _ => Err("the gateway returned an unexpected status response".to_owned()),
        }
    }
}

impl std::fmt::Debug for HostCustody {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.label())
    }
}

impl HostCustody {
    #[cfg(all(
        test,
        feature = "codex",
        feature = "grok",
        feature = "claude",
        feature = "test-hooks",
        feature = "web"
    ))]
    pub(crate) fn in_process_for_test(custody: GatewayCustody, home: &PlakPaths) -> Self {
        Self::Gateway(Arc::new(GatewayLink {
            admin: AdminLink {
                address: GatewayAddress::Unix(home.home().join("unused-admin.sock")),
                key: None,
            },
            custody,
        }))
    }

    /// Reads `[gateway]` from `home`'s `config.toml`. A config that does not
    /// load, an address that does not parse and a key file that is unsafe
    /// are errors.
    pub(crate) fn resolve(home: &PlakPaths) -> Result<Self, String> {
        let config = ProductConfig::load_home_file(home)?;
        let Some(text) = config
            .gateway_address
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        else {
            return Ok(Self::Local);
        };
        let address = super::address::parse(text, config.gateway_allow_non_tailnet)
            .map_err(|error| format!("gateway.address: {error}"))?;
        let key = caller_key(home, &address, config.gateway_key_file.as_deref())?;
        let endpoint = match &address {
            GatewayAddress::Unix(path) => GatewayEndpoint::Unix(path.clone()),
            GatewayAddress::Tcp(addr) => GatewayEndpoint::Tcp(*addr),
        };
        let relay_key = key
            .as_ref()
            .map(|text| SensitiveBytes::new(text.clone().into_bytes()));
        let custody = GatewayCustody::connect(endpoint, relay_key, credential_headers)
            .map_err(|error| format!("credential gateway: {error}"))?;
        Ok(Self::Gateway(Arc::new(GatewayLink {
            admin: AdminLink { address, key },
            custody,
        })))
    }

    /// The gateway's relay custody, when one is configured.
    pub(crate) fn gateway(&self) -> Option<&GatewayCustody> {
        match self {
            Self::Local => None,
            Self::Gateway(link) => Some(&link.custody),
        }
    }

    /// The admin link, when a gateway is configured.
    pub(crate) fn admin(&self) -> Option<&AdminLink> {
        match self {
            Self::Local => None,
            Self::Gateway(link) => Some(&link.admin),
        }
    }

    pub(crate) fn address(&self) -> Option<&GatewayAddress> {
        self.admin().map(|link| &link.address)
    }

    /// `gateway: in-process` or the configured address.
    pub(crate) fn label(&self) -> String {
        self.address()
            .map_or_else(|| "in-process".to_owned(), GatewayAddress::label)
    }

    /// Route custody for every route constructor and credentials provider.
    pub(crate) fn route_custody(&self) -> plakous::custody::Custody {
        self.gateway()
            .map_or_else(plakous::custody::Custody::local, GatewayCustody::custody)
    }

    /// `subscriptions` with their logins held by the gateway, or unchanged.
    /// The store is not opened when a gateway holds the logins.
    pub(crate) fn subscriptions(
        &self,
        subscriptions: plakous::subscriptions::NativeSubscriptions,
    ) -> plakous::subscriptions::NativeSubscriptions {
        match self.gateway() {
            Some(gateway) => {
                super::subscription_custody::subscriptions_under(subscriptions, gateway)
            }
            None => subscriptions,
        }
    }

    /// `roots` whose sessions' logins the gateway holds, or unchanged.
    pub(crate) fn roots(
        &self,
        roots: plakous_hosted::agent::ProductStateRoots,
    ) -> plakous_hosted::agent::ProductStateRoots {
        match self.gateway() {
            Some(gateway) => super::subscription_custody::roots_under(roots, gateway),
            None => roots,
        }
    }

    /// A unix gateway must be the exclusive holder of `state_home`'s store
    /// and advertise exactly the configured socket; otherwise the host would
    /// relay through a gateway that serves some other store. A tcp gateway
    /// is another machine's and is not checked here.
    pub(crate) fn verify_holder(&self, state_home: &Path) -> Result<(), String> {
        let Some(GatewayAddress::Unix(socket)) = self.address() else {
            return Ok(());
        };
        let lock = super::holder::lock_path(state_home);
        let advertised = super::holder::advertised_socket(&lock).map_err(|error| {
            format!(
                "cannot read the credential holder at {}: {error}",
                lock.display()
            )
        })?;
        match advertised {
            Some(found) if found == *socket => Ok(()),
            Some(found) => Err(format!(
                "the gateway holding {} serves {}, not the configured {}; point [gateway] address at that socket or set gateway.toml socket and state_root for this home",
                lock.display(),
                found.display(),
                socket.display()
            )),
            None => Err(format!(
                "no plakd gateway serve holds {}; start the gateway for this state home (gateway.toml state_root) before this host",
                lock.display()
            )),
        }
    }
}

/// The caller key a gateway call presents. A unix gateway gets a key only
/// when `gateway.key_file` names one; a same-UID unix peer without a key is
/// the local administrator. A tcp gateway always needs one, by default
/// `<home>/secrets/gateway.key`. The file must be owner-only.
fn caller_key(
    home: &PlakPaths,
    address: &GatewayAddress,
    key_file: Option<&str>,
) -> Result<Option<String>, String> {
    let path = match (address, key_file) {
        (_, Some(path)) => PathBuf::from(path),
        (GatewayAddress::Tcp(_), None) => home.secrets_dir().join("gateway.key"),
        (GatewayAddress::Unix(_), None) => return Ok(None),
    };
    super::read_key_file(&path)
        .map(Some)
        .map_err(|error| format!("gateway caller key {}: {error}", path.display()))
}

fn credential_headers(kind: &str) -> Option<&'static [&'static str]> {
    openai::headers(kind)
        .or_else(|| anthropic::headers(kind))
        .or_else(|| xai::headers(kind))
        .or_else(|| openrouter::headers(kind))
        .or_else(|| codex::headers(kind))
        .or_else(|| claude::headers(kind))
        .or_else(|| grok::headers(kind))
}

#[cfg(feature = "openai")]
mod openai {
    pub(super) fn headers(kind: &str) -> Option<&'static [&'static str]> {
        (kind == "openai").then_some(plakous::gateway::pins::openai::OPENAI_API_CREDENTIAL_HEADERS)
    }
}

#[cfg(not(feature = "openai"))]
mod openai {
    pub(super) fn headers(_kind: &str) -> Option<&'static [&'static str]> {
        None
    }
}

#[cfg(feature = "anthropic")]
mod anthropic {
    pub(super) fn headers(kind: &str) -> Option<&'static [&'static str]> {
        (kind == "anthropic")
            .then_some(plakous::gateway::pins::anthropic::ANTHROPIC_API_CREDENTIAL_HEADERS)
    }
}

#[cfg(not(feature = "anthropic"))]
mod anthropic {
    pub(super) fn headers(_kind: &str) -> Option<&'static [&'static str]> {
        None
    }
}

#[cfg(feature = "xai-api")]
mod xai {
    pub(super) fn headers(kind: &str) -> Option<&'static [&'static str]> {
        (kind == "xai-api").then_some(plakous::gateway::pins::xai::XAI_API_CREDENTIAL_HEADERS)
    }
}

#[cfg(not(feature = "xai-api"))]
mod xai {
    pub(super) fn headers(_kind: &str) -> Option<&'static [&'static str]> {
        None
    }
}

#[cfg(feature = "openrouter")]
mod openrouter {
    pub(super) fn headers(kind: &str) -> Option<&'static [&'static str]> {
        (kind == "openrouter")
            .then_some(plakous::gateway::pins::openrouter::OPENROUTER_CREDENTIAL_HEADERS)
    }
}

#[cfg(not(feature = "openrouter"))]
mod openrouter {
    pub(super) fn headers(_kind: &str) -> Option<&'static [&'static str]> {
        None
    }
}

#[cfg(feature = "codex")]
mod codex {
    pub(super) fn headers(kind: &str) -> Option<&'static [&'static str]> {
        (kind == "codex").then_some(plakous::gateway::pins::openai::CODEX_CREDENTIAL_HEADERS)
    }
}

#[cfg(not(feature = "codex"))]
mod codex {
    pub(super) fn headers(_kind: &str) -> Option<&'static [&'static str]> {
        None
    }
}

#[cfg(feature = "claude")]
mod claude {
    pub(super) fn headers(kind: &str) -> Option<&'static [&'static str]> {
        (kind == "claude").then_some(plakous::gateway::pins::anthropic::CLAUDE_CREDENTIAL_HEADERS)
    }
}

#[cfg(not(feature = "claude"))]
mod claude {
    pub(super) fn headers(_kind: &str) -> Option<&'static [&'static str]> {
        None
    }
}

#[cfg(feature = "grok")]
mod grok {
    pub(super) fn headers(kind: &str) -> Option<&'static [&'static str]> {
        (kind == "grok").then_some(plakous::gateway::pins::xai::SUPERGROK_CREDENTIAL_HEADERS)
    }
}

#[cfg(not(feature = "grok"))]
mod grok {
    pub(super) fn headers(_kind: &str) -> Option<&'static [&'static str]> {
        None
    }
}

#[cfg(test)]
mod tests;
