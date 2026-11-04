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
/// for its whole life, taken before the socket is touched, and writes its
/// process id into it while it holds it: a second daemon
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
    check_socket_path(socket)?;
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
    // The lock file names its holder, so an operator or a harness can find
    // the daemon that serves a socket; the lock proves the name current.
    {
        use std::io::Write as _;
        let mut holder = &lock;
        holder.set_len(0)?;
        write!(holder, "{}", std::process::id())?;
        holder.sync_all()?;
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
    use tokio::io::AsyncWriteExt as _;
    check_socket_path(socket)?;
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
    // Everything the daemon sent reaches the host, flushed, before the
    // bridge ends: a daemon that refuses a host says why and then closes,
    // and the host must read the refusal, not only the closed link.
    let delivered = async {
        let copied = tokio::io::copy(&mut from_daemon, &mut stdout).await;
        let flushed = stdout.flush().await;
        copied.and(flushed.map(|()| 0))
    };
    tokio::pin!(delivered);
    let copied = tokio::select! {
        // The daemon went away: the host sees its last frames, then the end.
        copied = &mut delivered => copied,
        // The host hung up, or the daemon stopped reading. The daemon sees
        // the end of its input and closes; what it sent until then is
        // still delivered.
        sent = tokio::io::copy(&mut stdin, &mut to_daemon) => {
            let _ = to_daemon.shutdown().await;
            match tokio::time::timeout(DRAIN, &mut delivered).await {
                Ok(copied) => copied.and(sent),
                Err(_) => sent,
            }
        }
    };
    match copied {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error),
    }
}

/// How long the bridge still delivers the daemon's output after the host's
/// side ended.
const DRAIN: Duration = Duration::from_secs(5);

/// The longest socket path the platform's `sockaddr_un` holds, with its
/// terminating NUL.
#[cfg(target_os = "macos")]
const SOCKET_PATH_LIMIT: usize = 104;
#[cfg(not(target_os = "macos"))]
const SOCKET_PATH_LIMIT: usize = 108;

/// Refuses a socket path the platform cannot bind or connect to, naming it,
/// rather than failing with an opaque error from the kernel. The daemon and
/// the bridge check it first; a caller checks it before it prepares
/// anything for a daemon.
///
/// # Errors
///
/// The path is too long for this platform's `sockaddr_un`.
pub fn check_socket_path(socket: &Path) -> io::Result<()> {
    let length = socket.as_os_str().len();
    if length < SOCKET_PATH_LIMIT {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "the runner socket path {} is {length} bytes, past this platform's limit of {}; \
             use a shorter product home",
            socket.display(),
            SOCKET_PATH_LIMIT - 1
        ),
    ))
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

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;

    #[test]
    fn a_socket_path_past_the_platform_limit_is_refused_by_name() {
        let short = Path::new("/tmp/run/v1-0123456789ab.sock");
        assert!(check_socket_path(short).is_ok());
        let long = PathBuf::from(format!("/tmp/{}/v1-0123456789ab.sock", "d".repeat(100)));
        let refused = check_socket_path(&long).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::InvalidInput);
        assert!(refused.to_string().contains(&long.display().to_string()));
    }
}
