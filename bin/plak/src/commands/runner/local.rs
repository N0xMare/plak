//! The host's journaled connection to its home-local process daemon.

mod start;

use std::sync::Arc;
use std::time::Duration;

use plakous::runner::{ClientWindows, ConnectError, FileCallJournal, RunnerClient};
use plakous_hosted::error::{CliFailure, CliResult};

use super::home::{LocalRunnerHome, private_directory};
use crate::home::paths::PlakPaths;

/// Old protocols are inspected, never transferred or used for execution.
pub(crate) async fn older(
    paths: &PlakPaths,
) -> CliResult<Vec<plakous::runner::protocol::LineStatus>> {
    let mut lines = Vec::new();
    for protocol in 1..super::version().protocol {
        let socket = paths
            .home()
            .join("host/runner")
            .join(format!("v{protocol}/run/runner.sock"));
        let stream = match tokio::net::UnixStream::connect(socket).await {
            Ok(stream) => stream,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                continue;
            }
            Err(error) => {
                tracing::warn!(target: "plak.runner", phase = "inspection", protocol, error = %error, "runner_recovery_failed");
                continue;
            }
        };
        let host_id = match super::host_id(paths) {
            Ok(id) => id,
            Err(error) => {
                tracing::warn!(target: "plak.runner", phase = "inspection", protocol, error = %error, "runner_recovery_failed");
                continue;
            }
        };
        let mut bind = super::bind(host_id, true);
        bind.version.protocol = protocol;
        let inspect = async {
            let (client, _) = match RunnerClient::connect_home_local(
                stream,
                bind,
                ClientWindows::default(),
                None,
            )
            .await
            {
                Ok(connected) => connected,
                // An obsolete daemon can idle-exit with this inspection queued.
                Err(ConnectError::Closed) => return Ok(Vec::new()),
                Err(error) => return Err(CliFailure::new(error.to_string())),
            };
            let result = tokio::time::timeout(super::START_WAIT, client.lines()).await;
            client.disconnect();
            result
                .map_err(|_| CliFailure::new("the old runner inspection timed out"))?
                .map_err(|error| {
                    CliFailure::new(format!("the old runner inspection failed: {error:?}"))
                })
        };
        match tokio::time::timeout(super::START_WAIT, inspect).await {
            Ok(Ok(inspected)) => lines.extend(inspected),
            Ok(Err(error)) => {
                tracing::warn!(target: "plak.runner", phase = "inspection", protocol, error = %error, "runner_recovery_failed");
            }
            Err(error) => {
                tracing::warn!(target: "plak.runner", phase = "inspection", protocol, error = %error, "runner_recovery_failed");
            }
        }
    }
    Ok(lines)
}

/// Dials before starting, and retries both daemon-exit races under one bound.
pub(crate) async fn dial(paths: &PlakPaths) -> CliResult<RunnerClient> {
    let home = LocalRunnerHome::of(paths);
    plakous::runner::daemon::check_socket_path(&home.socket_path())
        .map_err(|error| CliFailure::new(error.to_string()))?;
    private_directory(home.root())?;
    let journal = Arc::new(
        FileCallJournal::open(
            home.root()
                .join("calls")
                .join(crate::home::state::state_key()),
        )
        .map_err(|error| CliFailure::new(format!("the local runner journal failed: {error}")))?,
    );
    let bind = super::bind(super::host_id(paths)?, false);
    let attempt = async {
        let mut starter = start::Starter::new();
        loop {
            if let Ok(stream) = tokio::net::UnixStream::connect(home.socket_path()).await {
                match RunnerClient::connect_home_local(
                    stream,
                    bind.clone(),
                    ClientWindows::default(),
                    Some(journal.clone()),
                )
                .await
                {
                    Ok((client, _)) => return Ok(client),
                    // An idle daemon can exit with a connection queued.
                    Err(ConnectError::Closed) => {}
                    Err(error) => return Err(CliFailure::new(error.to_string())),
                }
            } else {
                check_start(&home)?;
                // Socket removal precedes lock release. A busy lock is
                // not permission to spawn another daemon.
                let _ = starter.start(paths, &home).await;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    tokio::time::timeout(super::START_WAIT, attempt)
        .await
        .map_err(|_| CliFailure::new("the home-local runner did not answer within 10 s"))?
}

/// Reconnects the existing client so every session retains the same transport.
pub(crate) async fn reconnect(paths: &PlakPaths, client: &RunnerClient) -> CliResult<()> {
    let home = LocalRunnerHome::of(paths);
    let attempt = async {
        let mut starter = start::Starter::new();
        loop {
            if let Ok(stream) = tokio::net::UnixStream::connect(home.socket_path()).await {
                match client.reconnect(stream).await {
                    Ok(_) => {
                        return Ok(());
                    }
                    Err(ConnectError::Closed) => {}
                    Err(error) => return Err(CliFailure::new(error.to_string())),
                }
            } else {
                check_start(&home)?;
                let _ = starter.start(paths, &home).await;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    tokio::time::timeout(super::START_WAIT, attempt)
        .await
        .map_err(|_| CliFailure::new("the home-local runner did not reconnect within 10 s"))?
}

fn check_start(home: &LocalRunnerHome) -> CliResult<()> {
    match plakous::runner::FileOwnerFloors::open(home.floors_path()) {
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::InvalidData
                    | std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::InvalidInput
                    | std::io::ErrorKind::IsADirectory
                    | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Err(CliFailure::new(format!(
                "the home-local runner refused {}: {error}",
                home.floors_path().display()
            )))
        }
        _ => Ok(()),
    }
}
