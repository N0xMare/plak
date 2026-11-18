//! Where an account command sends its call.
//!
//! 1. `[gateway] address`, when set.
//! 2. The socket a local service advertised in the holder lock.
//! 3. Otherwise the command stays in this process.

use std::path::Path;

use super::address::{self, GatewayAddress};
use super::holder;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AccountRoute {
    /// A configured gateway. Output names it.
    Configured(GatewayAddress),
    /// The local service. Output names it.
    Service(GatewayAddress),
    /// No gateway is configured and no service holds the store.
    InProcess,
}

impl AccountRoute {
    pub(crate) fn remote(&self) -> Option<&GatewayAddress> {
        match self {
            Self::Configured(address) | Self::Service(address) => Some(address),
            Self::InProcess => None,
        }
    }

    pub(crate) fn via_line(&self) -> Option<String> {
        self.remote()
            .map(|address| format!("via gateway {}", address.label()))
    }
}

pub(crate) fn select(
    configured: Option<&str>,
    allow_non_tailnet: bool,
    lock_path: &Path,
) -> Result<AccountRoute, String> {
    if let Some(text) = configured.map(str::trim).filter(|text| !text.is_empty()) {
        let address = address::parse(text, allow_non_tailnet).map_err(|error| error.to_string())?;
        return Ok(AccountRoute::Configured(address));
    }
    match holder::advertised_socket(lock_path) {
        Ok(Some(path)) => Ok(AccountRoute::Service(GatewayAddress::Unix(path))),
        Ok(None) => Ok(AccountRoute::InProcess),
        Err(error) if holder::is_busy(&error) => Err(
            "the credential store is held, but the holder did not advertise a socket; set [gateway] address"
                .to_owned(),
        ),
        Err(error) => Err(error.to_string()),
    }
}

/// Provider key variables a gateway client must not carry. The gateway's own
/// environment is the one that resolves keys.
pub(crate) const PROVIDER_KEY_ENV: [&str; 4] = [
    "OPENAI_API_KEY",
    "XAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "OPENROUTER_API_KEY",
];

pub(crate) fn provider_key_env_set() -> Vec<&'static str> {
    PROVIDER_KEY_ENV
        .into_iter()
        .filter(|name| std::env::var_os(name).is_some())
        .collect()
}
