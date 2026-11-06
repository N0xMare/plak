//! The private listener the web endpoint binds: a unix socket in a 0700 directory
//! (the `tailscale serve unix:<path>` deployment), or 127.0.0.1 in dev mode,
//! and the HTTP/1.1 connections it accepts.

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use plakous_hosted::error::{CliFailure, CliResult};

use super::Served;

/// How long a connection may take to send its request head.
const HEADER_DEADLINE: Duration = Duration::from_secs(10);
/// The most HTTP connections open at once, authenticated or not.
const MAX_CONNECTIONS: usize = 128;
/// The read buffer one connection may hold (its request head).
const MAX_REQUEST_BUFFER: usize = 64 * 1024;
/// How often the accept loop looks at the session and the interrupt count.
const WATCH: Duration = Duration::from_millis(200);

/// Where the web endpoint listens.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Listen {
    /// A unix socket; its directory is made 0700 and must be the user's.
    Unix(PathBuf),
    /// A loopback TCP port, dev mode only.
    Loopback(SocketAddr),
}

impl Listen {
    /// Parses `unix:<path>` or `127.0.0.1:<port>`.
    ///
    /// # Errors
    ///
    /// Any other address: a TCP listener binds loopback only.
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        if let Some(path) = value.strip_prefix("unix:") {
            let path = PathBuf::from(path);
            if !path.is_absolute() || path.file_name().is_none() {
                return Err(format!(
                    "--listen {value}: a unix socket path must be absolute"
                ));
            }
            return Ok(Self::Unix(path));
        }
        let address: SocketAddr = value
            .parse()
            .map_err(|_| format!("--listen {value}: expected unix:<path> or 127.0.0.1:<port>"))?;
        if address.ip() != std::net::IpAddr::from([127, 0, 0, 1]) {
            return Err(format!(
                "--listen {value}: a TCP listener binds 127.0.0.1 only"
            ));
        }
        Ok(Self::Loopback(address))
    }
}

impl fmt::Display for Listen {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unix(path) => write!(formatter, "unix:{}", path.display()),
            Self::Loopback(address) => write!(formatter, "{address}"),
        }
    }
}

/// A bound listener.
pub(crate) enum Bound {
    Unix(tokio::net::UnixListener),
    Loopback(tokio::net::TcpListener),
}

/// The unix socket this process bound, by the file's identity, so cleanup
/// never removes a socket another process bound at the same path later.
pub(crate) struct SocketFile {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

/// Binds `listen`. A unix socket's directory is created 0700, or must already
/// be a 0700 directory of this user. A socket already at the path is
/// replaced only when nothing accepts on it (stale); a live one is refused.
///
/// # Errors
///
/// A directory that is not private, a socket in use, or a bind that fails.
pub(crate) async fn bind(listen: &Listen) -> CliResult<(Bound, Option<SocketFile>)> {
    match listen {
        Listen::Unix(path) => {
            let directory = path.parent().ok_or_else(|| {
                CliFailure::new("plak host --web: the socket path has no directory")
            })?;
            private_directory(directory)?;
            match std::fs::symlink_metadata(path) {
                Ok(metadata) if is_socket(&metadata) => {
                    match std::os::unix::net::UnixStream::connect(path) {
                        Ok(_) => {
                            return Err(CliFailure::new(format!(
                                "plak host --web: {} is in use by a running endpoint",
                                path.display()
                            )));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                            std::fs::remove_file(path).map_err(|_| {
                                CliFailure::new(
                                    "plak host --web: could not replace the stale socket",
                                )
                            })?;
                        }
                        Err(_) => {
                            return Err(CliFailure::new(format!(
                                "plak host --web: cannot tell whether {} is in use",
                                path.display()
                            )));
                        }
                    }
                }
                Ok(_) => {
                    return Err(CliFailure::new(format!(
                        "plak host --web: {} exists and is not a socket",
                        path.display()
                    )));
                }
                Err(_) => {}
            }
            let listener = tokio::net::UnixListener::bind(path).map_err(|error| {
                CliFailure::new(format!(
                    "plak host --web: could not bind {}: {error}",
                    path.display()
                ))
            })?;
            let metadata = std::fs::symlink_metadata(path).map_err(|_| {
                CliFailure::new("plak host --web: could not inspect the bound socket")
            })?;
            let file = {
                use std::os::unix::fs::MetadataExt as _;
                SocketFile {
                    path: path.clone(),
                    dev: metadata.dev(),
                    ino: metadata.ino(),
                }
            };
            Ok((Bound::Unix(listener), Some(file)))
        }
        Listen::Loopback(address) => {
            let listener = tokio::net::TcpListener::bind(address)
                .await
                .map_err(|error| {
                    CliFailure::new(format!(
                        "plak host --web: could not bind {address}: {error}"
                    ))
                })?;
            Ok((Bound::Loopback(listener), None))
        }
    }
}

#[cfg(unix)]
fn is_socket(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt as _;
    metadata.file_type().is_socket()
}

/// Creates `directory` 0700, or checks that it is a 0700 directory owned by
/// this user and not a symlink.
fn private_directory(directory: &Path) -> CliResult<()> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
    if std::fs::symlink_metadata(directory).is_err() {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(directory)
            .map_err(|_| {
                CliFailure::new("plak host --web: could not create the socket directory")
            })?;
    }
    let metadata = std::fs::symlink_metadata(directory)
        .map_err(|_| CliFailure::new("plak host --web: could not inspect the socket directory"))?;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(CliFailure::new(format!(
            "plak host --web: {} must be a 0700 directory of this user",
            directory.display()
        )));
    }
    Ok(())
}

/// Removes the unix socket this process bound, and only that one.
pub(crate) fn cleanup(file: Option<SocketFile>) {
    use std::os::unix::fs::MetadataExt as _;
    let Some(file) = file else {
        return;
    };
    if std::fs::symlink_metadata(&file.path)
        .is_ok_and(|metadata| metadata.dev() == file.dev && metadata.ino() == file.ino)
    {
        let _ = std::fs::remove_file(&file.path);
    }
}

/// Accepts and serves connections until an interrupt arrives; a session
/// that released itself is woken by the next connection.
pub(crate) async fn serve(listener: Bound, served: Arc<Served>) {
    let open = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let mut watch = tokio::time::interval(WATCH);
    loop {
        tokio::select! {
            _ = watch.tick() => {
                if served.stop.stopped() {
                    return;
                }
            }
            accepted = accept(&listener) => {
                // Past the cap a connection is closed at once: nothing
                // before authentication can hold more than this many.
                if let Some(io) = accepted
                    && let Ok(permit) = Arc::clone(&open).try_acquire_owned()
                {
                    let served = Arc::clone(&served);
                    tokio::spawn(async move {
                        connection(io, served).await;
                        drop(permit);
                    });
                }
            }
        }
    }
}

/// One accepted stream, boxed so both listeners share one connection path.
type Accepted = Box<dyn ConnectionIo>;

trait ConnectionIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin> ConnectionIo for T {}

async fn accept(listener: &Bound) -> Option<Accepted> {
    match listener {
        Bound::Unix(listener) => listener
            .accept()
            .await
            .ok()
            .map(|(stream, _)| Box::new(stream) as Accepted),
        Bound::Loopback(listener) => listener
            .accept()
            .await
            .ok()
            .map(|(stream, _)| Box::new(stream) as Accepted),
    }
}

async fn connection(io: Accepted, served: Arc<Served>) {
    let service = service_fn(move |request| {
        let served = Arc::clone(&served);
        async move { Ok::<_, std::convert::Infallible>(super::auth::handle(request, served)) }
    });
    let _ = http1::Builder::new()
        .max_buf_size(MAX_REQUEST_BUFFER)
        .header_read_timeout(HEADER_DEADLINE)
        .timer(hyper_util::rt::TokioTimer::new())
        .serve_connection(TokioIo::new(io), service)
        .with_upgrades()
        .await;
}
