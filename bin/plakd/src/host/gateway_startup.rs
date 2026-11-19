use std::io;
use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::time::{self, Instant};

use crate::gateway::client;
use crate::gateway::{GatewayAddress, GatewayUnavailable, HostCustody, verify_holder};

const STARTUP_WAIT: Duration = Duration::from_secs(3);
const STARTUP_RETRY: Duration = Duration::from_millis(25);

/// Require the configured listener before a host acquires any runtime authority.
pub(super) async fn verify(custody: &HostCustody, state_home: &Path) -> Result<(), String> {
    let admin = custody
        .admin()
        .ok_or_else(|| GatewayUnavailable::MissingAddress.to_string())?;
    let address = &admin.address;
    if let GatewayAddress::Unix(socket) = address {
        wait_for_listener(socket, || verify_holder(address, state_home)).await?;
    }
    let request = client::request_head(admin.key(), "GET", client::VERSIONS_PATH, 0)?;
    let reply = time::timeout(STARTUP_WAIT, async {
        match address {
            GatewayAddress::Unix(socket) => {
                read_versions(tokio::net::UnixStream::connect(socket).await?, &request).await
            }
            GatewayAddress::Tcp(socket) => {
                read_versions(tokio::net::TcpStream::connect(socket).await?, &request).await
            }
        }
    })
    .await
    .ok()
    .and_then(Result::ok)
    .ok_or_else(|| GatewayUnavailable::Unreachable(address).to_string())?;
    client::supported_version(&client::decode_call(client::parse_response(&reply)?)?)?;
    Ok(())
}

async fn read_versions(
    mut stream: impl AsyncRead + AsyncWrite + Unpin,
    request: &str,
) -> io::Result<Vec<u8>> {
    stream.write_all(request.as_bytes()).await?;
    let mut response = Vec::new();
    stream.take(8192).read_to_end(&mut response).await?;
    Ok(response)
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
