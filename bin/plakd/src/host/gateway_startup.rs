use std::io;
use std::path::Path;
use std::time::Duration;

use tokio::time::{self, Instant};

use crate::gateway::{GatewayAddress, GatewayUnavailable, HostCustody, verify_holder};
use plakous::gateway::{RelayCallError, RelayClient};

const STARTUP_WAIT: Duration = Duration::from_secs(3);
const STARTUP_RETRY: Duration = Duration::from_millis(25);

/// Require the configured listener before a host acquires any runtime authority.
pub(super) async fn verify(custody: &HostCustody, state_home: &Path) -> Result<(), String> {
    let address = custody
        .address()
        .ok_or_else(|| GatewayUnavailable::MissingAddress.to_string())?;
    if let GatewayAddress::Unix(socket) = address {
        wait_for_listener(socket, || verify_holder(address, state_home)).await?;
    }
    let relay = custody
        .gateway()
        .ok_or_else(|| GatewayUnavailable::MissingAddress.to_string())?
        .custodian()
        .relay();
    let control = RelayClient::call_control();
    match time::timeout(STARTUP_WAIT, relay.version(&control)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(RelayCallError::VersionMismatch { supported })) => Err(format!(
            "credential gateway protocol mismatch: serves {supported:?}"
        )),
        Ok(Err(error @ RelayCallError::Refused(_))) => Err(error.to_string()),
        _ => Err(GatewayUnavailable::Unreachable(address).to_string()),
    }
}

async fn wait_for_listener(
    socket: &Path,
    prove_holder: impl Fn() -> Result<bool, String>,
) -> Result<(), String> {
    let deadline = Instant::now() + STARTUP_WAIT;
    loop {
        if prove_holder()? {
            match time::timeout_at(deadline, tokio::net::UnixStream::connect(socket)).await {
                Ok(Ok(_stream)) => return Ok(()),
                Ok(Err(error)) => {
                    if !matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        time::sleep_until((now + STARTUP_RETRY).min(deadline)).await;
    }
    Err(GatewayUnavailable::Unreachable(&GatewayAddress::Unix(socket.to_path_buf())).to_string())
}

#[cfg(test)]
mod tests;
