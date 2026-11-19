//! Where an account command sends its call.
//!
//! 1. `[gateway] address`, when set.
//! 2. The socket a local service advertised in the holder lock.
//! 3. Otherwise the command stays in this process.

use std::path::Path;

use super::address::GatewayAddress;
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

/// Where an account command goes: the configured gateway, else a local
/// service that advertised its socket, else this process. Discovery does
/// not create the holder file.
pub(crate) fn select(
    custody: &super::HostCustody,
    lock_path: &Path,
) -> Result<AccountRoute, String> {
    if let Some(address) = custody.address() {
        return Ok(AccountRoute::Configured(address.clone()));
    }
    if !lock_path.is_file() {
        return Ok(AccountRoute::InProcess);
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
