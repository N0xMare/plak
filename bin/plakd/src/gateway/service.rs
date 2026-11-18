//! The gateway process: exclusive holder, listeners, one exchange per connection.

use std::future::Future;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use plakous::gateway::protocol::UuidText;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, UnixListener};

use super::audit::Audit;
use super::auth::Peer;
use super::bridge::AccountBridge;
use super::callers::Callers;
use super::dispatch::{self, GatewayState};
use super::flags::Flags;
use super::holder::{self, HolderLock, HolderMode};
use super::http;
use super::io_file::ensure_private_dir;
use super::server_config::{self, ServerConfig};
use crate::home::paths::PlakPaths;

pub(crate) struct Gateway {
    state: Arc<GatewayState>,
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
        let instance =
            UuidText::new(uuid::Uuid::new_v4().to_string()).map_err(|error| error.to_string())?;
        let callers = Callers::load(&server_config::callers_path(home))
            .map_err(|error| format!("callers.json: {error}"))?;
        let flags = Flags::load(&server_config::flags_path(home))?;
        let audit = Audit::open(server_config::audit_dir(home), config.audit_retention_days)
            .map_err(|error| error.to_string())?;
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
        });
        Ok(Self { state, lock })
    }

    /// Foreground serve until SIGTERM, Ctrl-C or an admin drain.
    pub(crate) fn serve_foreground(self) -> Result<(), String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let state = Arc::clone(&self.state);
        let stop_state = Arc::clone(&state);
        runtime.block_on(async move {
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
            run(state, stop).await
        })?;
        drop(self.lock);
        Ok(())
    }
}

pub(crate) async fn run(
    state: Arc<GatewayState>,
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
    let max_body = state.config.max_request_body_bytes;
    let unix_state = Arc::clone(&state);
    let tcp_state = Arc::clone(&state);
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
                let uid = stream.peer_cred().map(|cred| cred.uid()).ok();
                let peer = Peer {
                    uid_matches: uid == Some(rustix::process::geteuid().as_raw()),
                    address: None,
                };
                let task_state = Arc::clone(&unix_state);
                tokio::spawn(async move {
                    let _ = serve_stream(task_state, peer, stream, max_body).await;
                });
            }
            accepted = accept_tcp(tcp.as_ref()), if tcp.is_some() => {
                let Ok((stream, peer_addr)) = accepted else { continue; };
                let peer = Peer {
                    uid_matches: false,
                    address: Some(peer_addr.ip()),
                };
                let task_state = Arc::clone(&tcp_state);
                tokio::spawn(async move {
                    let _ = serve_stream(task_state, peer, stream, max_body).await;
                });
            }
        }
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
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

async fn serve_stream<S>(
    state: Arc<GatewayState>,
    peer: Peer,
    mut stream: S,
    max_body: u64,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Ok(request) = http::read_request(&mut stream, max_body).await else {
        let response = dispatch::refusal_response(
            &plakous::gateway::protocol::refusal::GatewayRefusal::EnvelopeInvalid {},
        );
        http::write_response(&mut stream, &response).await?;
        return Ok(());
    };
    let body_len = u64::try_from(request.body.len()).unwrap_or(u64::MAX);
    if body_len > max_body {
        let response = dispatch::refusal_response(
            &plakous::gateway::protocol::refusal::GatewayRefusal::LimitExceeded {},
        );
        return http::write_response(&mut stream, &response).await;
    }
    let task_state = Arc::clone(&state);
    let response =
        tokio::task::spawn_blocking(move || dispatch::handle(&task_state, &peer, &request))
            .await
            .map_err(|error| std::io::Error::other(error.to_string()))?;
    if matches_draining(&response) {
        state.draining.store(true, Ordering::Relaxed);
        state.shutdown.notify_one();
    }
    http::write_response(&mut stream, &response).await
}

fn matches_draining(response: &http::Response) -> bool {
    response.status == 200
        && response
            .body
            .windows(10)
            .any(|window| window == b"\"draining\"")
}

fn prepare_socket(path: &PathBuf) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent).map_err(|error| error.to_string())?;
    }
    if path.exists() {
        std::fs::remove_file(path).map_err(|error| error.to_string())?;
    }
    Ok(())
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
