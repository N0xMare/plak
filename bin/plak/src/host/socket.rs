//! The host's control socket (spec 04 §4.7): `<home>/host/run/<state
//! key>.sock`, 0600 in a 0700 `run/` under a 0700 `host/`. Only the user who
//! owns the directory may speak on it; anyone else's connection is closed
//! before a byte is read. A socket found there is replaced only by the
//! holder of `host.lock`, so a live host's socket is never unlinked.
//!
//! Each admitted connection speaks wire v2 lines, exactly stdio's (one
//! frame per line, at most 16 MiB), to the host's sessions: its hello is
//! answered with the host's welcome, which names no session, and then it
//! is a `HostConnection` of the host (spec 02 §4.4, the session
//! directory). It is a remote face: nothing on it execs or hands off the
//! host's build. A client that leaves, or one that falls 10 s behind on a
//! write, detaches; its sessions run on.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use plakous_host_protocol::wire::{HandshakeRefusal, LineReader, WireError};
use plakous_hosted::error::{CliFailure, CliResult};
use plakous_hosted::host_service::wire::{
    Face, HELLO_DEADLINE, SessionHost, handshake, lines, refuse,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};

use super::lock::HostLock;
use crate::surface_protocol::PlakExtension;

/// How often the accept loop and every connection look for a stop.
const STOP_POLL: Duration = Duration::from_millis(50);
/// The most connections waiting for their hello at once; past it a
/// connection is closed unanswered.
const PENDING_LIMIT: usize = 16;
/// The most admitted connections at once; past it a hello is refused
/// `busy`.
const CONNECTION_LIMIT: usize = 64;
/// How long the accept loop waits after a failed accept (out of
/// descriptors, say) before it tries again.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// How long one write may take to reach a slow client before it is
/// dropped.
const SEND_DEADLINE: Duration = Duration::from_secs(10);
/// The most bytes waiting for one client before it is dropped as slow.
const MAX_PENDING_BYTES: usize = 64 * 1024 * 1024;

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
        Self::bind_owned(path)
    }

    // The caller already owns this path under the home's live lock.
    fn bind_owned(path: &Path) -> CliResult<Self> {
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

    /// Accepts connections to `host` until `stopped` turns true. Each
    /// admitted one is served on a task of its own; a stop ends them as
    /// leaving clients.
    pub(super) async fn serve(
        &mut self,
        host: &Arc<SessionHost<PlakExtension>>,
        stopped: impl Fn() -> bool + Send + Sync + 'static,
        idle: Option<super::idle::IdleExit>,
        scanning: Arc<AtomicBool>,
    ) {
        let stopped: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(stopped);
        let mut tick = tokio::time::interval(STOP_POLL);
        let pending = Arc::new(AtomicUsize::new(0));
        let admitted = Arc::new(AtomicUsize::new(0));
        let mut next_idle_check = Instant::now();
        let mut idle = idle.map(super::idle::IdleGuard::new);
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
                    if let Some(idle) = &mut idle {
                        idle.activity();
                    }
                    if let Some(place) = Place::take(&pending, PENDING_LIMIT) {
                        let (host, stopped) = (Arc::clone(host), Arc::clone(&stopped));
                        let admitted = Arc::clone(&admitted);
                        tokio::spawn(async move {
                            connection(stream, &host, stopped, place, &admitted).await;
                        });
                    }
                }
                _ = tick.tick() => {
                    if stopped() {
                        return;
                    }
                    let now = Instant::now();
                    if now >= next_idle_check && let Some(idle) = &mut idle {
                        next_idle_check = now + Duration::from_secs(1);
                        let blocked = pending.load(Ordering::Acquire) != 0
                            || admitted.load(Ordering::Acquire) != 0
                            || scanning.load(Ordering::Acquire)
                            || host.idle_residents_blocked();
                        if idle.expired(now, blocked, host).await {
                            match self.retire_idle_listener() {
                                Ok(backlog) if backlog.is_empty() => {
                                    eprintln!("plak host: auto-started service idle for five minutes; stopping");
                                    return;
                                }
                                Ok(backlog) => {
                                    idle.activity();
                                    for stream in backlog {
                                        if !admits(stream.peer_cred().ok().map(|peer| peer.uid()), self.owner) {
                                            continue;
                                        }
                                        if let Some(place) = Place::take(&pending, PENDING_LIMIT) {
                                            let (host, stopped) = (Arc::clone(host), Arc::clone(&stopped));
                                            let admitted = Arc::clone(&admitted);
                                            tokio::spawn(async move {
                                                connection(stream, &host, stopped, place, &admitted).await;
                                            });
                                        }
                                    }
                                }
                                Err(error) => {
                                    eprintln!("plak host: idle socket retirement failed: {error}");
                                    idle.activity();
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Hide discovery first, then drain the kernel backlog without waiting.
    /// The home lock remains held throughout, including a cancelled exit's rebind.
    fn retire_idle_listener(&mut self) -> CliResult<Vec<UnixStream>> {
        use std::os::fd::AsFd as _;
        let failed = |error| CliFailure::new(format!("cannot retire idle socket: {error}"));
        let duplicate = self.listener.as_fd().try_clone_to_owned().map_err(failed)?;
        let listener = std::os::unix::net::UnixListener::from(duplicate);
        std::fs::remove_file(&self.path).map_err(failed)?;
        let mut backlog = Vec::new();
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(true).map_err(failed)?;
                    backlog.push(UnixStream::from_std(stream).map_err(failed)?);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(failed(error)),
            }
        }
        if !backlog.is_empty() {
            *self = Self::bind_owned(&self.path)?;
        }
        Ok(backlog)
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

/// One connection's place under a count's limit, given back when dropped.
struct Place(Arc<AtomicUsize>);

impl Place {
    /// A place, or `None` when `limit` are already taken.
    fn take(count: &Arc<AtomicUsize>, limit: usize) -> Option<Self> {
        count
            .try_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                (held < limit).then_some(held + 1)
            })
            .ok()
            .map(|_| Self(Arc::clone(count)))
    }
}

impl Drop for Place {
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

/// The frames one connection has written and not yet sent.
#[derive(Clone, Default)]
struct Outbox(Arc<Mutex<Vec<u8>>>);

impl Outbox {
    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl Write for Outbox {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut pending = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if pending.len() + buf.len() > MAX_PENDING_BYTES {
            return Err(std::io::Error::other("the client fell behind"));
        }
        pending.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Sends what the connection wrote; `Err` when the client is gone or too
/// slow.
async fn send(write: &mut OwnedWriteHalf, outbox: &Outbox) -> Result<(), ()> {
    let bytes = outbox.take();
    if bytes.is_empty() {
        return Ok(());
    }
    match tokio::time::timeout(SEND_DEADLINE, write.write_all(&bytes)).await {
        Ok(Ok(())) => Ok(()),
        _ => Err(()),
    }
}

/// The client's lines as they arrive.
struct Reader {
    read: tokio::net::unix::OwnedReadHalf,
    lines: LineReader,
    queued: VecDeque<Result<String, WireError>>,
    buffer: Vec<u8>,
}

impl Reader {
    /// Reads once; `false` at the end of input.
    async fn fill(&mut self) -> bool {
        match self.read.read(&mut self.buffer).await {
            Ok(0) | Err(_) => false,
            Ok(read) => {
                self.queued.extend(self.lines.push(&self.buffer[..read]));
                true
            }
        }
    }
}

/// Serves one admitted connection: its hello, then its frames and its
/// sessions' observations, until it leaves or the host stops.
async fn connection(
    stream: UnixStream,
    host: &Arc<SessionHost<PlakExtension>>,
    stopped: Arc<dyn Fn() -> bool + Send + Sync>,
    pending: Place,
    admitted: &Arc<AtomicUsize>,
) {
    let Ok(credentials) = stream.peer_cred() else {
        return;
    };
    let uid = credentials.uid();
    let (read, mut write) = stream.into_split();
    let mut reader = Reader {
        read,
        lines: LineReader::default(),
        queued: VecDeque::new(),
        buffer: vec![0; 64 * 1024],
    };
    let outbox = Outbox::default();
    let hello = tokio::time::timeout(HELLO_DEADLINE, async {
        while reader.queued.is_empty() {
            if !reader.fill().await {
                return None;
            }
        }
        reader.queued.pop_front()
    })
    .await;
    let hello = match hello {
        Ok(Some(hello)) => hello,
        Ok(None) => return,
        Err(_) => {
            let _ = refuse::<PlakExtension>(
                &mut outbox.clone(),
                HandshakeRefusal::Malformed {
                    detail: format!("no hello within {} ms", HELLO_DEADLINE.as_millis()),
                },
            );
            let _ = send(&mut write, &outbox).await;
            return;
        }
    };
    let Some(place) = Place::take(admitted, CONNECTION_LIMIT) else {
        let _ = refuse::<PlakExtension>(&mut outbox.clone(), HandshakeRefusal::Busy);
        let _ = send(&mut write, &outbox).await;
        return;
    };
    drop(pending);
    let (first, hellos) = lines(1);
    let _ = first.send(hello);
    drop(first);
    let stop = Arc::clone(&stopped);
    let principal = plakous_hosted::host_service::wire::HostPrincipal::Socket(uid);
    let admitted = handshake::<PlakExtension>(
        &hellos,
        host.welcome_for(&principal),
        &mut outbox.clone(),
        &move || stop(),
    );
    if send(&mut write, &outbox).await.is_err() || !matches!(admitted, Ok(true)) {
        return;
    }
    let stop = Arc::clone(&stopped);
    let mut connection = host.connect(outbox.clone(), Face::Remote, principal, move || stop());
    loop {
        // A flush may read the session directory from the stores.
        let flushed = tokio::task::block_in_place(|| connection.flush());
        if flushed.is_err() || send(&mut write, &outbox).await.is_err() || stopped() {
            break;
        }
        if let Some(line) = reader.queued.pop_front() {
            // An attach may open a session, which takes a while.
            let reading = tokio::task::block_in_place(|| connection.frame(line));
            if !matches!(reading, Ok(true)) {
                let _ = connection.flush();
                let _ = send(&mut write, &outbox).await;
                break;
            }
            continue;
        }
        tokio::select! {
            more = reader.fill() => {
                if !more {
                    break;
                }
            }
            () = connection.changed() => {}
            () = tokio::time::sleep(STOP_POLL) => {}
        }
    }
    // Every way out detaches, so no session holds a dead attachment.
    connection.leave();
    let _ = write.shutdown().await;
    drop(place);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
