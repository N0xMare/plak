//! The runner daemon (spec 01 §8.4): one [`RunnerService`] behind a private
//! socket for its machine user, and the stdio bridge a host reaches it
//! through (`ssh … plak runner connect --stdio`).
//!
//! The socket lives in a directory only its owner can enter and carries
//! mode 0600; a connection from another user is closed before the hello.
//! The socket's name is keyed by the protocol version and the build's
//! version triple, so two builds never answer each other's hosts.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use plakous_runner_protocol::VersionTriple;
use sha2::{Digest as _, Sha256};
use tokio::net::{UnixListener, UnixStream};

use crate::service::RunnerService;

/// Why the daemon did not serve.
#[derive(Debug)]
#[non_exhaustive]
pub enum DaemonError {
    /// Another daemon of the same version triple already answers on the
    /// socket.
    AlreadyServing(PathBuf),
    /// The socket or its directory could not be prepared or served.
    Io(io::Error),
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyServing(path) => {
                write!(formatter, "a runner already serves on {}", path.display())
            }
            Self::Io(error) => write!(formatter, "the runner socket failed: {error}"),
        }
    }
}

impl std::error::Error for DaemonError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::AlreadyServing(_) => None,
            Self::Io(error) => Some(error),
        }
    }
}

impl From<io::Error> for DaemonError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// The socket a runner of `version` listens on in `run_directory`. Socket
/// paths are short (about 100 bytes), so the triple is hashed.
#[must_use]
pub fn socket_path(run_directory: &Path, version: &VersionTriple) -> PathBuf {
    let digest = Sha256::digest(
        format!(
            "{}\0{}\0{}",
            version.protocol, version.source_revision, version.flavor
        )
        .as_bytes(),
    );
    let mut name = format!("v{}-", version.protocol);
    for byte in &digest[..6] {
        use std::fmt::Write as _;
        let _ = write!(name, "{byte:02x}");
    }
    run_directory.join(format!("{name}.sock"))
}

/// Serves `service` on `socket` until the runner is idle for `idle_grace`:
/// no connection, no job or orphan, no service and no unacknowledged answer
/// (spec 01 §8.4). The socket is removed on the way out.
///
/// The daemon holds an exclusive lock on the socket's lock file beside it
/// for its whole life, taken before the socket is touched: a second daemon
/// of the same version never unlinks or binds a live one's socket, and a
/// socket found while holding the lock is a dead daemon's.
///
/// # Errors
///
/// Another daemon already holds the lock, or the socket cannot be
/// prepared.
pub async fn serve_socket(
    service: RunnerService,
    socket: &Path,
    idle_grace: Duration,
) -> Result<(), DaemonError> {
    let directory = socket.parent().unwrap_or_else(|| Path::new("."));
    private_directory(directory)?;
    let lock = {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(socket.with_extension("lock"))?
    };
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(DaemonError::AlreadyServing(socket.to_path_buf()));
        }
        Err(std::fs::TryLockError::Error(error)) => return Err(DaemonError::Io(error)),
    }
    if std::fs::symlink_metadata(socket).is_ok() {
        // A daemon that died left its socket behind.
        std::fs::remove_file(socket)?;
    }
    let listener = UnixListener::bind(socket)?;
    set_mode(socket, 0o600)?;
    let owner = owner_uid(directory)?;
    let mut idle_since: Option<Instant> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let result = loop {
        tokio::select! {
            accepted = listener.accept() => {
                let stream = match accepted {
                    Ok((stream, _)) => stream,
                    Err(error) => break Err(DaemonError::Io(error)),
                };
                // Only the runner's own user may speak to it.
                if stream.peer_cred().is_ok_and(|peer| peer.uid() == owner) {
                    idle_since = None;
                    let service = service.clone();
                    tokio::spawn(async move { service.serve_connection(stream).await });
                }
            }
            _ = tick.tick() => {
                if !service.is_idle() {
                    idle_since = None;
                } else if idle_since.get_or_insert_with(Instant::now).elapsed() >= idle_grace {
                    break Ok(());
                }
            }
        }
    };
    // The socket goes before the lock: a daemon that takes the lock next
    // finds no socket of this one.
    let _ = std::fs::remove_file(socket);
    drop(lock);
    result
}

/// Bridges this process's stdin and stdout to the daemon on `socket`
/// until either side closes. When no daemon answers, `start` is called once
/// to start one detached, and the socket is tried again for up to `wait`.
///
/// # Errors
///
/// No daemon answered, `start` failed, or the bridge failed.
pub async fn bridge_stdio(
    socket: &Path,
    start: impl FnOnce() -> io::Result<()>,
    wait: Duration,
) -> io::Result<()> {
    let mut stream = if let Ok(stream) = UnixStream::connect(socket).await {
        stream
    } else {
        start()?;
        let deadline = Instant::now() + wait;
        loop {
            match UnixStream::connect(socket).await {
                Ok(stream) => break stream,
                Err(error) if Instant::now() >= deadline => return Err(error),
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    };
    let (mut from_daemon, mut to_daemon) = stream.split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    // Either side closing ends the bridge: the host's ssh hung up, or the
    // daemon went away and the host must see its link end.
    let copied = tokio::select! {
        copied = tokio::io::copy(&mut stdin, &mut to_daemon) => copied,
        copied = tokio::io::copy(&mut from_daemon, &mut stdout) => copied,
    };
    match copied {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error),
    }
}

/// Creates `directory` (mode 0700) or checks an existing one is the
/// caller's own and closed to everyone else.
fn private_directory(directory: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(directory)?;
    let metadata = std::fs::symlink_metadata(directory)?;
    if !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory",
            directory.display()
        )));
    }
    if metadata.mode() & 0o077 != 0 {
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

fn owner_uid(directory: &Path) -> io::Result<u32> {
    use std::os::unix::fs::MetadataExt as _;
    Ok(std::fs::metadata(directory)?.uid())
}
