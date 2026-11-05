//! The host's control socket (spec 04 §4.7): `<home>/host/run/<state
//! key>.sock`, 0600 in a 0700 `run/` under a 0700 `host/`. Only the user who
//! owns the directory may speak on it; anyone else's connection is closed
//! before a byte is read. A socket found there is replaced only by the
//! holder of `host.lock`, so a live host's socket is never unlinked.
//!
//! The skeleton hosts no session: every admitted connection gets one wire
//! v2 `refused unavailable` after its hello (or after the hello deadline),
//! and is closed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use plakous_host_protocol::wire::HandshakeRefusal;
use plakous_hosted::error::{CliFailure, CliResult};
use plakous_hosted::host_service::wire::{HELLO_DEADLINE, refuse};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{UnixListener, UnixStream};

use super::lock::HostLock;
use crate::surface_protocol::PlakExtension;

/// What every admitted connection is told while no session is hosted.
pub(super) const NO_SESSION: &str = "plak host serves no session yet";
/// How often the accept loop looks for a stop.
const STOP_POLL: Duration = Duration::from_millis(50);
/// The most a hello may carry before the line is given up on.
const HELLO_LIMIT: usize = 16 * 1024 * 1024;
/// The most connections waiting for their hello at once; past it a
/// connection is closed unanswered.
const PENDING_LIMIT: usize = 16;
/// How long the accept loop waits after a failed accept (out of
/// descriptors, say) before it tries again.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// The longest socket path the platform's `sockaddr_un` holds, with its
/// terminating NUL.
#[cfg(target_os = "macos")]
const SOCKET_PATH_LIMIT: usize = 104;
#[cfg(not(target_os = "macos"))]
const SOCKET_PATH_LIMIT: usize = 108;

/// The bound control socket.
pub(super) struct Bound {
    listener: UnixListener,
    path: PathBuf,
    /// The uid that owns the socket's directory: the only peer admitted.
    owner: u32,
    /// The bound socket file, so cleanup removes only this one.
    file: (u64, u64),
}

impl Bound {
    /// Binds `path`, replacing a socket a host that died left there; the
    /// caller holds `host.lock`, which is what makes that safe.
    ///
    /// # Errors
    ///
    /// The path is too long for this platform, a directory cannot be made
    /// private, or the socket cannot be bound.
    pub(super) fn bind(path: &Path, _held: &HostLock) -> CliResult<Self> {
        use std::os::unix::fs::MetadataExt as _;
        check_length(path)?;
        let run = path
            .parent()
            .ok_or_else(|| CliFailure::new("the host socket has no directory"))?;
        if let Some(host) = run.parent() {
            crate::commands::runner::private_directory(host)?;
        }
        crate::commands::runner::private_directory(run)?;
        let failed = |error: std::io::Error| {
            CliFailure::new(format!("{} could not be bound: {error}", path.display()))
        };
        if std::fs::symlink_metadata(path).is_ok() {
            std::fs::remove_file(path).map_err(failed)?;
        }
        let listener = std::os::unix::net::UnixListener::bind(path).map_err(failed)?;
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(failed)?;
        }
        listener.set_nonblocking(true).map_err(failed)?;
        let listener = UnixListener::from_std(listener).map_err(failed)?;
        let owner = std::fs::metadata(run).map_err(failed)?.uid();
        let metadata = std::fs::symlink_metadata(path).map_err(failed)?;
        Ok(Self {
            listener,
            path: path.to_path_buf(),
            owner,
            file: (metadata.dev(), metadata.ino()),
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// Accepts connections until `stopped` turns true. Each admitted one is
    /// answered on a task of its own; a stop does not wait for them.
    pub(super) async fn serve(&self, stopped: impl Fn() -> bool) {
        let mut tick = tokio::time::interval(STOP_POLL);
        let pending = Arc::new(AtomicUsize::new(0));
        loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    let Ok((stream, _)) = accepted else {
                        tokio::time::sleep(ACCEPT_BACKOFF).await;
                        continue;
                    };
                    if !admits(stream.peer_cred().ok().map(|peer| peer.uid()), self.owner) {
                        continue;
                    }
                    if let Some(slot) = Pending::take(&pending) {
                        tokio::spawn(async move {
                            answer(stream).await;
                            drop(slot);
                        });
                    }
                }
                _ = tick.tick() => {
                    if stopped() {
                        return;
                    }
                }
            }
        }
    }

    /// Removes the socket file when it is still the one this host bound.
    pub(super) fn remove(&self) {
        use std::os::unix::fs::MetadataExt as _;
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == self.file)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// One connection's place among those waiting for their hello.
struct Pending(Arc<AtomicUsize>);

impl Pending {
    /// A place, or `None` when [`PENDING_LIMIT`] connections already wait.
    fn take(count: &Arc<AtomicUsize>) -> Option<Self> {
        count
            .try_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                (held < PENDING_LIMIT).then_some(held + 1)
            })
            .ok()
            .map(|_| Self(Arc::clone(count)))
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Whether a peer may speak on the socket: only its directory's owner, and
/// never a peer whose credentials cannot be read.
fn admits(peer: Option<u32>, owner: u32) -> bool {
    peer == Some(owner)
}

/// Refuses a path the platform cannot bind, naming it.
fn check_length(path: &Path) -> CliResult<()> {
    let length = path.as_os_str().len();
    if length < SOCKET_PATH_LIMIT {
        return Ok(());
    }
    Err(CliFailure::new(format!(
        "the host socket path {} is {length} bytes, past this platform's limit of {}; \
         use a shorter product home",
        path.display(),
        SOCKET_PATH_LIMIT - 1
    )))
}

/// Waits for the hello line (bounded by the hello deadline and the frame
/// cap), then refuses `unavailable` and closes.
async fn answer(mut stream: UnixStream) {
    let mut line = Vec::new();
    let mut buffer = [0_u8; 4096];
    let _ = tokio::time::timeout(HELLO_DEADLINE, async {
        while !line.contains(&b'\n') && line.len() < HELLO_LIMIT {
            match stream.read(&mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(read) => line.extend_from_slice(&buffer[..read]),
            }
        }
    })
    .await;
    let mut frame = Vec::new();
    if refuse::<PlakExtension>(
        &mut frame,
        HandshakeRefusal::Unavailable {
            detail: NO_SESSION.to_owned(),
        },
    )
    .is_ok()
    {
        let _ = stream.write_all(&frame).await;
    }
    let _ = stream.shutdown().await;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
