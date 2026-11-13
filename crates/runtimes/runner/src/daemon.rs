//! The runner daemon (spec 01 §8.4): one [`RunnerService`] behind a private
//! socket for its machine user, and the stdio bridge a host reaches it
//! through (`ssh … plak runner connect --stdio`).
//!
//! The socket lives in a directory only its owner can enter and carries
//! mode 0600; a connection from another user is closed before the hello.
//! The socket's name is keyed by the protocol version and the build's
//! version triple, so two builds never answer each other's hosts.

use std::io;
use std::os::unix::fs::MetadataExt as _;
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
    run_directory.join(format!("{}.sock", triple_key(version)))
}

/// Filesystem key of one exact protocol/revision/flavor triple. Mutable daemon
/// floors, lock, socket and log must all live beneath this same key.
#[must_use]
pub fn triple_key(version: &VersionTriple) -> String {
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
    name
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "isolated daemon-state fixtures")]
mod triple_tests {
    use super::*;
    use crate::{FileOwnerFloors, Floor, OwnerFloors};
    use plakous_runner_protocol::LineOwner;

    #[test]
    fn each_triple_component_isolates_floors_socket_lock_and_log() {
        let home = tempfile::tempdir().unwrap();
        let current = VersionTriple::current("current", "debug");
        let versions = [
            current.clone(),
            VersionTriple {
                protocol: current.protocol + 1,
                ..current.clone()
            },
            VersionTriple {
                source_revision: "other".into(),
                ..current.clone()
            },
            VersionTriple {
                flavor: "release".into(),
                ..current.clone()
            },
        ];
        let mut keys = std::collections::BTreeSet::new();
        let mut live = Vec::new();
        private_directory(&home.path().join("builds")).unwrap();
        for (number, version) in versions.iter().enumerate() {
            let directory = home.path().join("builds").join(triple_key(version));
            assert!(keys.insert(directory.clone()));
            private_directory(&directory).unwrap();
            let floors = FileOwnerFloors::open(directory.join("owners.json")).unwrap();
            let floor = Floor {
                owner: LineOwner {
                    line: "same-line".into(),
                    generation: number as u64 + 1,
                    branch: "branch".into(),
                    fence: 1,
                },
                holder: "same-instance".into(),
                retention_only: false,
            };
            floors.record("same-host", &floor).unwrap();
            let socket = socket_path(&directory, version);
            let lock = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(socket.with_extension("lock"))
                .unwrap();
            lock.try_lock().unwrap();
            std::fs::write(directory.join("serve.log"), number.to_string()).unwrap();
            live.push((floors, floor, lock, directory));
        }
        for (number, (floors, expected, _lock, directory)) in live.iter().enumerate() {
            assert_eq!(floors.load(), [("same-host".into(), expected.clone())]);
            assert_eq!(
                FileOwnerFloors::open(directory.join("owners.json"))
                    .unwrap()
                    .load(),
                floors.load()
            );
            assert_eq!(
                std::fs::read_to_string(directory.join("serve.log")).unwrap(),
                number.to_string()
            );
        }
    }
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
    serve_socket_inner(service, socket, idle_grace, None).await
}

/// Serve the exact native socket and an independent frozen retention endpoint.
/// The build directory and its parents must already be owned and private.
///
/// # Errors
/// Refuses unsafe paths, another live daemon, or endpoint I/O failures.
pub async fn serve_socket_with_retention(
    service: RunnerService,
    socket: &Path,
    build_directory: &Path,
    idle_grace: Duration,
) -> Result<(), DaemonError> {
    serve_socket_inner(service, socket, idle_grace, Some(build_directory)).await
}

async fn serve_socket_inner(
    service: RunnerService,
    socket: &Path,
    idle_grace: Duration,
    retention_directory: Option<&Path>,
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
    take_lock(&lock, socket).await?;
    // Construction may predate the incumbent's last durable transfer. Only
    // exclusive native custody makes its final floors safe to cache.
    service.reload_owner_floors()?;
    // The lock file names its holder, so an operator or a harness can find
    // the daemon that serves a socket; the lock proves the name current.
    record_lock_holder(&lock)?;
    if std::fs::symlink_metadata(socket).is_ok() {
        // A daemon that died left its socket behind.
        std::fs::remove_file(socket)?;
    }
    let listener = UnixListener::bind(socket)?;
    set_mode(socket, 0o600)?;
    let endpoint = std::fs::symlink_metadata(socket)?;
    let owner = owner_uid(directory)?;
    let retention = retention_endpoint(retention_directory, &service, socket).await?;
    let mut retained = tokio::task::JoinSet::new();
    let mut connections = tokio::task::JoinSet::new();
    let mut idle_since: Option<Instant> = None;
    let mut reported_unknown = false;
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let result = loop {
        tokio::select! {
            accepted = async {
                match &retention {
                    Some(endpoint) => endpoint.listener.accept().await,
                    None => std::future::pending().await,
                }
            }, if retention.is_some() && retained.len() < 64 => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) if transient_accept(&error) => {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                    Err(error) => break Err(DaemonError::Io(error)),
                };
                if stream.peer_cred().is_ok_and(|peer| peer.uid() == owner)
                    && let Some(endpoint) = &retention {
                    let service = service.clone();
                    let origin = endpoint.origin.clone();
                    retained.spawn(async move { crate::retention::serve(service,stream,origin).await });
                }
            }
            _ = retained.join_next(), if !retained.is_empty() => {}
            accepted = listener.accept() => {
                let stream = match accepted {
                    Ok((stream, _)) => stream,
                    Err(error) if transient_accept(&error) => {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                    Err(error) => break Err(DaemonError::Io(error)),
                };
                // Only the runner's own user may speak to it.
                if stream.peer_cred().is_ok_and(|peer| peer.uid() == owner) {
                    idle_since = None;
                    let service = service.clone();
                    connections.spawn(async move { service.serve_connection(stream).await });
                }
            }
            _ = connections.join_next(), if !connections.is_empty() => {}
            _ = tick.tick() => {
                match same_endpoint(socket, &endpoint) {
                    Ok(false) if !service.retention_keeps_alive(Instant::now()) => break Ok(()),
                    Err(error) if !reported_unknown => {
                        use std::io::Write as _;
                        let _ = writeln!(io::stderr(), "runner endpoint could not be inspected; keeping it alive: {error}");
                        reported_unknown = true;
                    }
                    Ok(_) | Err(_) => {}
                }
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
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    retained.abort_all();
    while retained.join_next().await.is_some() {}
    service.shutdown().await;
    if matches!(same_endpoint(socket, &endpoint), Ok(true)) {
        let _ = std::fs::remove_file(socket);
    }
    drop(lock);
    result
}

fn record_lock_holder(lock: &std::fs::File) -> io::Result<()> {
    use std::io::Write as _;
    let mut holder = lock;
    holder.set_len(0)?;
    write!(holder, "{}", std::process::id())?;
    holder.sync_all()
}

fn same_endpoint(socket: &Path, original: &std::fs::Metadata) -> io::Result<bool> {
    match std::fs::symlink_metadata(socket) {
        Ok(current) => Ok(current.dev() == original.dev() && current.ino() == original.ino()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

async fn retention_endpoint(
    directory: Option<&Path>,
    service: &RunnerService,
    socket: &Path,
) -> Result<Option<crate::retention::Endpoint>, DaemonError> {
    let Some(directory) = directory else {
        return Ok(None);
    };
    let endpoint = crate::retention::Endpoint::bind(directory, service).await?;
    crate::retention::link_native_lock(directory, &socket.with_extension("lock"))?;
    Ok(Some(endpoint))
}

fn transient_accept(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted | io::ErrorKind::WouldBlock
    ) || [rustix::io::Errno::MFILE, rustix::io::Errno::NFILE]
        .iter()
        .any(|code| Some(code.raw_os_error()) == error.raw_os_error())
}

async fn take_lock(lock: &std::fs::File, socket: &Path) -> Result<(), DaemonError> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(DaemonError::AlreadyServing(socket.to_path_buf()));
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(DaemonError::Io(error)),
        }
    }
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
    builder.mode(0o700);
    match builder.create(directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = std::fs::symlink_metadata(directory)?;
    if !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory",
            directory.display()
        )));
    }
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} belongs to another user", directory.display()),
        ));
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

    fn service(root: &Path) -> RunnerService {
        RunnerService::new(
            plakous_tools::ExecutionEnvironment::host(
                plakous_tools::ExecutionEnvironmentScope::Run(
                    "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                ),
                root,
                ".",
                plakous_tools::ExecutionEnvironmentConfig::default(),
            )
            .unwrap(),
            crate::RunnerIdentity {
                runner_id: plakous_tools::RunnerId::new("endpoint-test").unwrap(),
                version: VersionTriple::current("test", "debug"),
                boot_id: "boot".into(),
                binary_id: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn transient_accept_faults_do_not_end_the_daemon() {
        for errno in [
            rustix::io::Errno::MFILE,
            rustix::io::Errno::NFILE,
            rustix::io::Errno::INTR,
            rustix::io::Errno::CONNABORTED,
        ] {
            assert!(transient_accept(&io::Error::from_raw_os_error(
                errno.raw_os_error()
            )));
        }
        assert!(!transient_accept(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
    }

    #[tokio::test]
    async fn an_unknown_endpoint_stat_does_not_stop_a_serving_daemon() {
        use std::os::unix::fs::PermissionsExt as _;
        if rustix::process::geteuid().is_root() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("run/runner.sock");
        let serving = serve_socket(service(root.path()), &socket, Duration::from_secs(300));
        tokio::pin!(serving);
        tokio::select! {
            result = &mut serving => panic!("daemon ended before binding: {result:?}"),
            () = async { while !socket.exists() { tokio::task::yield_now().await; } } => {}
        }
        let _connection = UnixStream::connect(&socket).await.unwrap();
        std::fs::set_permissions(
            socket.parent().unwrap(),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        let stayed = tokio::time::timeout(Duration::from_millis(650), &mut serving)
            .await
            .is_err();
        std::fs::set_permissions(
            socket.parent().unwrap(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        assert!(
            stayed,
            "an inaccessible endpoint is not evidence of deletion"
        );
        std::fs::remove_file(&socket).unwrap();
        tokio::time::timeout(Duration::from_secs(5), serving)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn runner_directory_creation_cannot_resurrect_a_missing_parent() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("missing-home");
        assert!(private_directory(&parent.join("run")).is_err());
        assert!(!parent.exists());
    }

    #[tokio::test]
    async fn a_daemon_exits_when_its_endpoint_disappears_or_is_replaced() {
        for replacement in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let socket = root.path().join("run/runner.sock");
            let serving = serve_socket(service(root.path()), &socket, Duration::from_secs(300));
            tokio::pin!(serving);
            tokio::select! {
                biased;
                result = &mut serving => panic!("daemon ended before binding: {result:?}"),
                () = async { while !socket.exists() { tokio::task::yield_now().await; } } => {}
            }
            // An open connection prevents ordinary idle exit, even with no jobs.
            let _connection = UnixStream::connect(&socket).await.unwrap();
            let successor = if replacement {
                std::fs::remove_file(&socket).unwrap();
                Some(UnixListener::bind(&socket).unwrap())
            } else {
                std::fs::remove_dir_all(socket.parent().unwrap()).unwrap();
                None
            };
            tokio::time::timeout(Duration::from_secs(5), &mut serving)
                .await
                .unwrap_or_else(|error| panic!("unreachable daemon must exit: {error}"))
                .unwrap();
            assert_eq!(socket.exists(), successor.is_some());
        }
    }

    #[tokio::test]
    async fn a_start_waits_out_a_concurrent_lock_probe() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runner.lock");
        let probe = std::fs::File::create(&path).unwrap();
        probe.lock().unwrap();
        let socket = directory.path().join("runner.sock");
        let service = RunnerService::new(
            plakous_tools::ExecutionEnvironment::host(
                plakous_tools::ExecutionEnvironmentScope::Run(
                    "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                ),
                directory.path(),
                ".",
                plakous_tools::ExecutionEnvironmentConfig::default(),
            )
            .unwrap(),
            crate::RunnerIdentity {
                runner_id: plakous_tools::RunnerId::new("probe-test").unwrap(),
                version: VersionTriple::current("test", "debug"),
                boot_id: "boot".into(),
                binary_id: None,
            },
        )
        .unwrap();
        let acquire = serve_socket(service, &socket, Duration::from_secs(1));
        tokio::pin!(acquire);
        tokio::select! {
            biased;
            result = &mut acquire => panic!("the probe must not refuse startup: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
        probe.unlock().unwrap();
        tokio::select! {
            biased;
            result = &mut acquire => panic!("the daemon must first bind its socket: {result:?}"),
            () = async { while !socket.exists() { tokio::task::yield_now().await; } } => {}
        }
        assert!(matches!(
            probe.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        acquire.await.unwrap();
        assert!(!socket.exists());
    }

    #[test]
    fn a_socket_path_past_the_platform_limit_is_refused_by_name() {
        let short = Path::new("/tmp/run/v1-0123456789ab.sock");
        assert!(check_socket_path(short).is_ok());
        let long = PathBuf::from(format!("/tmp/{}/v1-0123456789ab.sock", "d".repeat(100)));
        let refused = check_socket_path(&long).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::InvalidInput);
        assert!(refused.to_string().contains(&long.display().to_string()));
    }

    /// A socket directory another user owns is refused by name before
    /// anything is written there, for the machine runner and the home-local
    /// runner alike: both serve through `serve_socket`.
    #[tokio::test]
    async fn a_socket_directory_of_another_user_is_refused() {
        // Owned by root; a root test run proves nothing here.
        let theirs = Path::new("/");
        if rustix::process::geteuid().is_root() {
            return;
        }
        let refused = private_directory(theirs).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        let ours = tempfile::tempdir().unwrap();
        private_directory(&ours.path().join("run")).unwrap();
        private_directory(&ours.path().join("run")).unwrap();
        let service = RunnerService::new(
            plakous_tools::ExecutionEnvironment::host(
                plakous_tools::ExecutionEnvironmentScope::Run(
                    plakous_model::RunId::try_from(uuid::Uuid::new_v4()).unwrap(),
                ),
                ours.path(),
                ".",
                plakous_tools::ExecutionEnvironmentConfig::default(),
            )
            .unwrap(),
            crate::RunnerIdentity {
                runner_id: plakous_tools::RunnerId::new("daemon-test").unwrap(),
                version: VersionTriple::current("test", "debug"),
                boot_id: "boot".into(),
                binary_id: None,
            },
        )
        .unwrap();
        let Err(DaemonError::Io(refused)) = serve_socket(
            service,
            &theirs.join("plak-runner-test.sock"),
            Duration::ZERO,
        )
        .await
        else {
            panic!("a socket in another user's directory was served");
        };
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
    }
}
