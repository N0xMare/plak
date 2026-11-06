//! Same-user discovery distinguishes an absent host from its start/stop window.

use std::io::Read as _;
use std::time::{Duration, Instant};

use plakous_host_protocol::client::Transport;

use crate::{CliFailure, CliResult, product_paths::PlakPaths};

pub(super) fn connect(paths: &PlakPaths, window: Duration) -> CliResult<Option<Transport>> {
    let path = paths.host_socket();
    if std::os::unix::net::SocketAddr::from_pathname(&path).is_err() {
        return Ok(None);
    }
    let deadline = Instant::now() + window;
    loop {
        match crate::wire_transport::host_socket(&path) {
            Ok(transport) => return Ok(Some(transport)),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) => {}
            Err(error) => {
                return Err(CliFailure::new(format!(
                    "the host socket could not be reached: {error}"
                )));
            }
        }
        let Some(pid) = locked_host(paths)? else {
            return Ok(None);
        };
        if Instant::now() >= deadline {
            return Err(CliFailure::new(format!(
                "plak host (pid {pid}) is starting or stopping; retry"
            )));
        }
        std::thread::sleep(
            Duration::from_millis(25).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn locked_host(paths: &PlakPaths) -> CliResult<Option<String>> {
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(paths.host_lock())
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(CliFailure::new(format!(
                "the host lock could not be checked: {error}"
            )));
        }
    };
    match file.try_lock() {
        Ok(()) => Ok(None),
        Err(std::fs::TryLockError::WouldBlock) => {
            let mut holder = String::new();
            file.read_to_string(&mut holder).map_err(|error| {
                CliFailure::new(format!("the host lock holder could not be read: {error}"))
            })?;
            Ok(Some(
                holder
                    .split_whitespace()
                    .next()
                    .unwrap_or("unknown")
                    .to_owned(),
            ))
        }
        Err(std::fs::TryLockError::Error(error)) => Err(CliFailure::new(format!(
            "the host lock could not be checked: {error}"
        ))),
    }
}
