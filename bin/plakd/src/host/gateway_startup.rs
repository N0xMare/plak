use std::io;
use std::path::Path;
use std::time::Duration;

use tokio::time::{self, Instant};

use crate::gateway::{GatewayAddress, HolderProofError, HostCustody};

const STARTUP_WAIT: Duration = Duration::from_secs(3);
const STARTUP_RETRY: Duration = Duration::from_millis(25);

/// Service-manager ordering does not imply readiness. Prove the holder
/// and reach its Unix listener within one startup budget, sending no call.
pub(super) async fn verify(custody: &HostCustody, state_home: &Path) -> Result<(), String> {
    let Some(GatewayAddress::Unix(socket)) = custody.address() else {
        return Ok(());
    };
    wait_for_listener(socket, || custody.verify_holder(state_home)).await
}

async fn wait_for_listener(
    socket: &Path,
    prove_holder: impl Fn() -> Result<(), HolderProofError>,
) -> Result<(), String> {
    let deadline = Instant::now() + STARTUP_WAIT;
    loop {
        let message = match prove_holder() {
            Err(HolderProofError::Waiting(message)) => message,
            Err(HolderProofError::Refused(message)) => return Err(message),
            Ok(()) => {
                match time::timeout_at(deadline, tokio::net::UnixStream::connect(socket)).await {
                    Ok(Ok(_stream)) => return Ok(()),
                    Ok(Err(error)) => {
                        let message = format!(
                            "cannot connect to gateway socket {}: {error}",
                            socket.display()
                        );
                        if !matches!(
                            error.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                        ) {
                            return Err(message);
                        }
                        message
                    }
                    Err(_) => {
                        return Err(format!(
                            "timed out connecting to gateway socket {}",
                            socket.display()
                        ));
                    }
                }
            }
        };
        let now = Instant::now();
        if now >= deadline {
            return Err(message);
        }
        time::sleep_until((now + STARTUP_RETRY).min(deadline)).await;
    }
}

#[cfg(test)]
mod tests;
