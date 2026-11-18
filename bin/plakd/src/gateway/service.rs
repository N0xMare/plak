//! The gateway process: exclusive holder, listeners, one exchange per connection.

use std::future::Future;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use plakous::gateway::server::{Peer, RelayServer};
use tokio::net::{TcpListener, UnixListener};

use super::audit::Audit;
use super::bridge::AccountBridge;
use super::callers::Callers;
use super::dispatch::GatewayState;
use super::flags::Flags;
use super::holder::{self, HolderLock, HolderMode};
use super::io_file::ensure_private_dir;
use super::product::ProductGateway;
use super::server_config::{self, ServerConfig};
use crate::home::paths::PlakPaths;

pub(crate) struct Gateway {
    state: Arc<GatewayState>,
    server: RelayServer<ProductGateway>,
    lock: HolderLock,
}

impl Gateway {
    /// Opens the service files and takes the exclusive holder. Fails while any
    /// in-process holder exists.
    pub(crate) fn open(home: &PlakPaths, bridge: Box<dyn AccountBridge>) -> Result<Self, String> {
        let config = ServerConfig::load(home)?;
        Self::open_with(home, config, bridge)
    }

    pub(crate) fn open_with(
        home: &PlakPaths,
        config: ServerConfig,
        bridge: Box<dyn AccountBridge>,
    ) -> Result<Self, String> {
        let _ = ensure_private_dir(&ServerConfig::gateway_dir(home));
        let lock_path = holder::lock_path(&config.state_root);
        let lock = HolderLock::try_acquire(&lock_path, HolderMode::Exclusive).map_err(|error| {
            if holder::is_busy(&error) {
                "an in-process credential holder exists; stop it before plakd gateway serve"
                    .to_owned()
            } else {
                error.to_string()
            }
        })?;
        lock.advertise(&config.socket)
            .map_err(|error| error.to_string())?;
        let instance = plakous::gateway::server::new_instance()
            .ok_or_else(|| "could not mint a gateway instance".to_owned())?;
        let callers = Callers::load(&server_config::callers_path(home))
            .map_err(|error| format!("callers.json: {error}"))?;
        let flags = Flags::load(&server_config::flags_path(home))?;
        let audit = Audit::open(server_config::audit_dir(home), config.audit_retention_days)
            .map_err(|error| error.to_string())?
            .publishing_failures(server_config::audit_failures_path(home), instance.as_str());
        let state = Arc::new(GatewayState {
            instance,
            config,
            callers: Mutex::new(callers),
            flags: Mutex::new(flags),
            audit,
            bridge: Mutex::new(bridge),
            inflight: std::sync::atomic::AtomicU32::new(0),
            caller_inflight: Mutex::new(std::collections::HashMap::new()),
            draining: std::sync::atomic::AtomicBool::new(false),
            listeners: Mutex::new(Vec::new()),
            shutdown: tokio::sync::Notify::new(),
            relays: Mutex::new(std::collections::HashMap::new()),
        });
        let product = Arc::new(ProductGateway::new(home, Arc::clone(&state)));
        let upstream = plakous::gateway::server::http1_upstream()
            .map_err(|error| format!("gateway upstream client: {error}"))?;
        let bound = usize::try_from(state.config.max_request_body_bytes).unwrap_or(usize::MAX);
        let server = RelayServer::new(product, upstream, state.instance.clone())
            .with_max_request_body_bytes(bound);
        Ok(Self {
            state,
            server,
            lock,
        })
    }

    /// Foreground serve until SIGTERM, Ctrl-C or an admin drain. The holder
    /// lock is released only after in-flight relays and refresh claims
    /// settled (or their deadline passed) and the runtime shut down.
    pub(crate) fn serve_foreground(self) -> Result<(), String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let Self {
            state,
            server,
            lock,
        } = self;
        let stop_state = Arc::clone(&state);
        // The account bridge owns its own executor: the last reference to the
        // state must drop here, outside the gateway runtime's tasks.
        let held = Arc::clone(&state);
        let result = runtime.block_on(async move {
            let stop = async move {
                let mut terminate =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .map_err(|error| error.to_string())?;
                tokio::select! {
                    result = tokio::signal::ctrl_c() => {
                        result.map_err(|error| error.to_string())?;
                    }
                    _ = terminate.recv() => {}
                    () = stop_state.shutdown.notified() => {}
                }
                stop_state.draining.store(true, Ordering::Relaxed);
                Ok(())
            };
            run(state, server, stop).await
        });
        runtime.shutdown_timeout(RUNTIME_SHUTDOWN);
        drop(held);
        drop(lock);
        result
    }
}

/// How long in-flight relays and refresh claims may take to settle once the
/// service stops accepting.
const DRAIN_DEADLINE: Duration = Duration::from_secs(30);

/// How long the runtime's remaining tasks get after the drain deadline.
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(5);

pub(crate) async fn run(
    state: Arc<GatewayState>,
    server: RelayServer<ProductGateway>,
    stop: impl Future<Output = Result<(), String>>,
) -> Result<(), String> {
    prepare_socket(&state.config.socket)?;
    let unix = UnixListener::bind(&state.config.socket)
        .map_err(|error| format!("could not bind {}: {error}", state.config.socket.display()))?;
    std::fs::set_permissions(&state.config.socket, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| error.to_string())?;
    let tcp = match state.config.listen_tcp {
        Some(addr) => {
            if !super::address::interface_has(addr.ip()) {
                return Err(format!("listen_tcp {addr} is not assigned on this machine"));
            }
            let listener = TcpListener::bind(addr)
                .await
                .map_err(|error| format!("could not bind {addr}: {error}"))?;
            Some(listener)
        }
        None => None,
    };
    {
        let mut listeners = match state.listeners.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        listeners.push(format!("unix:{}", state.config.socket.display()));
        if let Some(addr) = state.config.listen_tcp {
            listeners.push(format!("tcp:{addr}"));
        }
    }
    let unix_server = server.clone();
    let tcp_server = server;
    tokio::pin!(stop);
    loop {
        if state.draining.load(Ordering::Relaxed) {
            break;
        }
        tokio::select! {
            result = &mut stop => {
                result?;
                break;
            }
            accepted = unix.accept() => {
                let Ok((stream, _)) = accepted else { continue; };
                let same_user = stream.peer_cred().map(|cred| cred.uid()).ok()
                    == Some(rustix::process::geteuid().as_raw());
                let peer = Peer::Unix { same_user };
                let task = unix_server.clone();
                tokio::spawn(async move {
                    let _ = task.serve(stream, peer).await;
                });
            }
            accepted = accept_tcp(tcp.as_ref()), if tcp.is_some() => {
                let Ok((stream, peer_addr)) = accepted else { continue; };
                let peer = Peer::Tcp(peer_addr);
                let task = tcp_server.clone();
                tokio::spawn(async move {
                    let _ = task.serve(stream, peer).await;
                });
            }
        }
    }
    // `inflight` counts every relay until its audit and every call, including
    // refresh claims, which run to completion on their own task even when
    // their caller leaves.
    let deadline = tokio::time::Instant::now() + DRAIN_DEADLINE;
    while state.inflight.load(Ordering::Relaxed) > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

async fn accept_tcp(
    listener: Option<&TcpListener>,
) -> std::io::Result<(tokio::net::TcpStream, SocketAddr)> {
    match listener {
        Some(listener) => listener.accept().await,
        None => std::future::pending().await,
    }
}

/// A stale socket of this user is replaced; anything else at the path is
/// refused, never unlinked.
fn prepare_socket(path: &PathBuf) -> Result<(), String> {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent).map_err(|error| error.to_string())?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta)
            if meta.file_type().is_socket()
                && meta.uid() == rustix::process::geteuid().as_raw() =>
        {
            std::fs::remove_file(path).map_err(|error| error.to_string())
        }
        Ok(_) => Err(format!(
            "{} exists and is not this user's socket; remove it or set another gateway.toml socket",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

impl std::fmt::Display for super::callers::CallerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exists => formatter.write_str("caller exists"),
            Self::Unknown => formatter.write_str("caller unknown"),
            Self::Io(message) | Self::Malformed(message) => formatter.write_str(message),
        }
    }
}
