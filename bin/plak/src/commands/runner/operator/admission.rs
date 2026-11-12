//! Existing daemon lock or a live paired link; no terminal pin file or auto-start.

use super::{CliFailure, CliResult, PlakPaths, home};
use plakous::runner::{ClientWindows, RunnerClient};

pub(crate) enum LocalReclamation {
    Quiet(std::fs::File),
    Live(RunnerClient),
}

pub(crate) async fn local_reclamation(paths: &PlakPaths) -> CliResult<LocalReclamation> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let socket = home::LocalRunnerHome::of(paths).socket_path();
    home::private_below(
        paths.home(),
        socket
            .parent()
            .ok_or_else(|| CliFailure::new("runner socket has no directory"))?,
    )?;
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(socket.with_extension("lock"))
        .map_err(|_| CliFailure::new("runner lock unavailable"))?;
    match lock.try_lock() {
        Ok(()) => return Ok(LocalReclamation::Quiet(lock)),
        Err(std::fs::TryLockError::WouldBlock) => {}
        Err(_) => return Err(CliFailure::new("runner lock unavailable")),
    }
    let bind = crate::commands::runner::bind(crate::commands::runner::host_id(paths)?, false);
    let connect = async {
        loop {
            match tokio::net::UnixStream::connect(&socket).await {
                Ok(stream) => {
                    return RunnerClient::connect_home_local(
                        stream,
                        bind.clone(),
                        ClientWindows::default(),
                        None,
                    )
                    .await
                    .map(|(client, _)| LocalReclamation::Live(client))
                    .map_err(|_| CliFailure::new("runner inventory link unavailable"));
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(_) => return Err(CliFailure::new("runner inventory link unavailable")),
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(3), connect)
        .await
        .map_err(|_| CliFailure::new("runner inventory link unavailable"))?
}
