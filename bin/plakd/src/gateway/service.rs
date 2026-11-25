//! The gateway process: exclusive holder, listeners, and bounded drain.

use std::future::Future;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use plakous::gateway::server::{Peer, RelayServer};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use super::audit::Audit;
use super::bridge::{AccountBridge, AccountThread, AccountWorker};
use super::callers::Callers;
use super::dispatch::GatewayState;
use super::flags::Flags;
#[cfg(test)]
use super::holder::HolderMode;
use super::holder::{self, HolderLock};
use super::io_file::ensure_private_dir;
use super::product::ProductGateway;
use super::server_config::{self, ServerConfig};
use crate::home::paths::PlakPaths;

pub(crate) struct Gateway {
    state: Arc<GatewayState>,
    server: RelayServer<ProductGateway>,
    accounts: AccountThread,
    lock: Arc<HolderLock>,
}

impl Gateway {
    /// Opens the service files and takes the exclusive holder.
    pub(crate) fn open_with(
        home: &PlakPaths,
        config: ServerConfig,
        bridge: Box<dyn AccountBridge>,
    ) -> Result<Self, String> {
        let _ = ensure_private_dir(&ServerConfig::gateway_dir(home));
        let lock_path = holder::lock_path(&config.state_root);
        let lock = HolderLock::acquire_service(&lock_path).map_err(|error| {
            if holder::is_busy(&error) {
                "the credential store already has a gateway; stop it before plakd gateway serve"
                    .to_owned()
            } else {
                error.to_string()
            }
        })?;
        let instance = plakous::gateway::server::new_instance()
            .ok_or_else(|| "could not mint a gateway instance".to_owned())?;
        let callers = Callers::load(&server_config::callers_path(home))
            .map_err(|error| format!("callers.json: {error}"))?;
        let flags = Flags::load(&server_config::flags_path(home))?;
        let audit = Audit::open(server_config::audit_dir(home), config.audit_retention_days)
            .map_err(|error| error.to_string())?
            .publishing_failures(server_config::audit_failures_path(home), instance.as_str());
        let upstream = plakous::gateway::server::http1_upstream()
            .map_err(|error| format!("gateway upstream client: {error}"))?;
        let lock = Arc::new(lock);
        let (bridge, accounts) = AccountWorker::start(bridge, Arc::clone(&lock))
            .map_err(|error| format!("could not start gateway account worker: {error}"))?;
        let state = Arc::new(GatewayState {
            instance,
            config,
            callers: Mutex::new(callers),
            flags: Mutex::new(flags),
            audit,
            bridge: Mutex::new(Box::new(bridge)),
            inflight: std::sync::atomic::AtomicU32::new(0),
            caller_inflight: Mutex::new(std::collections::HashMap::new()),
            draining: std::sync::atomic::AtomicBool::new(false),
            listeners: Mutex::new(Vec::new()),
            shutdown: tokio::sync::Notify::new(),
            next_relay_id: std::sync::atomic::AtomicU64::new(1),
            relays: Mutex::new(std::collections::HashMap::new()),
        });
        let product = Arc::new(ProductGateway::new(home, Arc::clone(&state)));
        let bound = usize::try_from(state.config.max_request_body_bytes).unwrap_or(usize::MAX);
        let server = RelayServer::new(product, upstream, state.instance.clone())
            .with_max_request_body_bytes(bound);
        let server = crate::test_hooks::gateway_loopback_anthropic(server)?;
        Ok(Self {
            state,
            server,
            accounts,
            lock,
        })
    }

    /// Foreground serve until SIGTERM, Ctrl-C or an admin drain. The holder
    /// lock is released only after in-flight relays and refresh claims
    /// settled (or their deadline passed), the runtime shuts down and the
    /// account worker joins.
    pub(crate) fn serve_foreground(self) -> Result<(), String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(GATEWAY_WORKER_THREADS)
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        self.serve_on(runtime)
    }

    pub(super) fn serve_on(self, runtime: tokio::runtime::Runtime) -> Result<(), String> {
        let Self {
            state,
            server,
            accounts,
            lock,
        } = self;
        let stop_state = Arc::clone(&state);
        let listening = Arc::clone(&lock);
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
            run(state, server, &listening, stop).await
        });
        let deadline = Instant::now() + RUNTIME_SHUTDOWN;
        accounts.close();
        runtime.shutdown_timeout(deadline.saturating_duration_since(Instant::now()));
        finish_accounts(accounts, lock, deadline)?;
        result
    }
}

fn finish_accounts(
    accounts: AccountThread,
    lock: Arc<HolderLock>,
    deadline: Instant,
) -> Result<(), String> {
    if accounts.join_until(deadline) {
        return Ok(());
    }
    // Keep the exclusive holder advertised until process exit even
    // if the detached account worker eventually finishes.
    std::mem::forget(lock);
    Err("gateway account worker did not stop before the shutdown deadline".to_owned())
}

/// Bounded Tokio worker pool for `plakd gateway serve` so idle servers do not
/// spin up one thread per host CPU core.
const GATEWAY_WORKER_THREADS: usize = 4;

/// Concurrent connection caps, split between Unix and TCP so a TCP connection
/// flood cannot lock out `local-admin` on the Unix socket.
const MAX_UNIX_CONNECTIONS: usize = 256;
const MAX_TCP_CONNECTIONS: usize = 256;

/// How long in-flight relays, refresh claims and admin calls may take to
/// settle once the service stops accepting.
const DRAIN_DEADLINE: Duration = Duration::from_secs(30);

/// How long the runtime's remaining tasks get after the drain deadline.
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(5);

async fn run(
    state: Arc<GatewayState>,
    server: RelayServer<ProductGateway>,
    lock: &HolderLock,
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
    lock.advertise(&state.config.socket)
        .map_err(|error| format!("could not advertise the gateway listener: {error}"))?;
    {
        let mut listeners = match state.listeners.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        listeners.push(format!("unix:{}", state.config.socket.display()));
        if let Some(listener) = &tcp {
            let addr = listener.local_addr().map_err(|error| error.to_string())?;
            listeners.push(format!("tcp:{addr}"));
        }
    }
    let unix_connections = Arc::new(Semaphore::new(MAX_UNIX_CONNECTIONS));
    let tcp_connections = Arc::new(Semaphore::new(MAX_TCP_CONNECTIONS));
    let mut connections = JoinSet::new();
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
            _ = connections.join_next(), if !connections.is_empty() => {}
            accepted = unix.accept() => {
                let Ok((stream, _)) = accepted else { continue; };
                let Ok(permit) = Arc::clone(&unix_connections).try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let same_user = stream.peer_cred().map(|cred| cred.uid()).ok()
                    == Some(rustix::process::geteuid().as_raw());
                let peer = Peer::Unix { same_user };
                let task = server.clone();
                connections.spawn(async move {
                    let _permit = permit;
                    let _ = task.serve(stream, peer).await;
                });
            }
            accepted = accept_tcp(tcp.as_ref()), if tcp.is_some() => {
                let Ok((stream, peer_addr)) = accepted else { continue; };
                let Ok(permit) = Arc::clone(&tcp_connections).try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let peer = Peer::Tcp(peer_addr);
                let task = server.clone();
                connections.spawn(async move {
                    let _permit = permit;
                    let _ = task.serve(stream, peer).await;
                });
            }
        }
    }
    // Stop new dials at connect, not after the caller has written into an
    // unserviced backlog. Already accepted connections still flush replies.
    drop(unix);
    drop(tcp);
    server.drain();
    state
        .listeners
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
    let _ = tokio::time::timeout(DRAIN_DEADLINE, finish_connections(connections, &state)).await;
    Ok(())
}

async fn finish_connections(mut connections: JoinSet<()>, state: &GatewayState) {
    // A call leaves the in-flight count before HTTP has flushed its reply.
    // Join the connections too, including the one that requested drain.
    while connections.join_next().await.is_some() {}
    // Detached claims may outlive a caller that closed its connection.
    while state.inflight.load(Ordering::Relaxed) > 0 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
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

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use plakous::gateway::protocol::GatewayAccount;
    use plakous::gateway::protocol::admin::{AdminRequest, AdminResponse};
    use plakous::gateway::protocol::facts::{AccountFacts, RefreshRequest};
    use plakous::gateway::protocol::refusal::GatewayRefusal;
    use plakous::gateway::protocol::{Endpoint, PROTOCOL_VERSION, call};
    use plakous::gateway::{GatewayEndpoint, RelayCallError, RelayClient};

    use super::*;

    struct HeldCall {
        started: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    impl AccountBridge for HeldCall {
        fn call(
            &mut self,
            _caller: &super::super::auth::Principal,
            _scopes: Option<&plakous::gateway::protocol::scope::ScopeSet>,
            _request: AdminRequest,
        ) -> AdminResponse {
            self.started.send(()).unwrap();
            self.release.recv_timeout(Duration::from_secs(10)).unwrap();
            AdminResponse::Accounts {
                accounts: Vec::new(),
            }
        }

        fn facts(
            &mut self,
            _accounts: &[GatewayAccount],
        ) -> Result<Vec<AccountFacts>, GatewayRefusal> {
            Err(GatewayRefusal::Internal {})
        }

        fn refresh(&mut self, _request: &RefreshRequest) -> Result<AccountFacts, GatewayRefusal> {
            Err(GatewayRefusal::Internal {})
        }
    }

    #[tokio::test]
    async fn product_gateway_closes_listeners_and_flushes_an_accepted_call_during_drain() {
        use std::io::Read as _;

        use tokio::io::AsyncWriteExt as _;

        let (_temp, home) = PlakPaths::isolated_test_home();
        let mut state = super::super::tests::state_for(&home);
        Arc::get_mut(&mut state).unwrap().config.listen_tcp = Some("127.0.0.1:0".parse().unwrap());
        let (started, entered) = tokio::sync::oneshot::channel();
        let (started_sync, entered_sync) = mpsc::channel();
        let (release, released) = mpsc::channel();
        *state.bridge.lock().unwrap() = Box::new(HeldCall {
            started: started_sync,
            release: released,
        });
        let transfer = tokio::task::spawn_blocking(move || {
            entered_sync.recv_timeout(Duration::from_secs(5)).unwrap();
            started.send(()).unwrap();
        });
        let server = RelayServer::new(
            Arc::new(ProductGateway::new(&home, Arc::clone(&state))),
            plakous::gateway::server::http1_upstream().unwrap(),
            state.instance.clone(),
        );
        let lock_path = holder::lock_path(&state.config.state_root);
        let lock = HolderLock::try_acquire(&lock_path, HolderMode::Exclusive).unwrap();
        let socket = state.config.socket.clone();
        let running = Arc::clone(&state);
        let service = tokio::spawn(async move {
            run(Arc::clone(&running), server, &lock, async {
                running.shutdown.notified().await;
                Ok(())
            })
            .await
        });
        let tcp = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(addr) = state.listeners.lock().unwrap().iter().find_map(|endpoint| {
                    endpoint
                        .strip_prefix("tcp:")
                        .map(|addr| addr.parse().unwrap())
                }) {
                    break addr;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut peer = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let list = call::encode(&AdminRequest::List {}).unwrap();
        let head = format!(
            "POST {} HTTP/1.1\r\nhost: gateway\r\ncontent-length: {}\r\n\r\n",
            Endpoint::Admin.path(PROTOCOL_VERSION),
            list.len(),
        );
        peer.write_all(head.as_bytes()).await.unwrap();
        peer.write_all(&list).await.unwrap();
        // Keep a nonblocking OS socket: after run returns the full reply and
        // EOF must already be readable, not depend on another reactor poll.
        let mut peer = peer.into_std().unwrap();
        let relay = RelayClient::new(GatewayEndpoint::Unix(socket), None).unwrap();
        let control = RelayClient::call_control();
        entered.await.unwrap();
        transfer.await.unwrap();
        let acknowledgement = relay
            .call(
                Endpoint::Admin,
                call::encode(&AdminRequest::Drain {}).unwrap(),
                &control,
            )
            .await
            .unwrap();
        assert_eq!(
            call::decode::<AdminResponse>(&acknowledgement).unwrap(),
            AdminResponse::Draining {}
        );
        assert_fresh_dials_are_unsent(&state, &relay, tcp, list).await;
        let Err(error) = HolderLock::try_acquire(&lock_path, HolderMode::Shared) else {
            panic!("the drain released credential custody early");
        };
        assert!(holder::is_busy(&error), "{error}");
        assert!(!service.is_finished());
        release.send(()).unwrap();
        service.await.unwrap().unwrap();
        let mut reply = String::new();
        peer.read_to_string(&mut reply).unwrap();
        let expected = call::encode(&AdminResponse::Accounts {
            accounts: Vec::new(),
        })
        .unwrap();
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        assert!(
            reply.contains(std::str::from_utf8(&expected).unwrap()),
            "{reply}"
        );
    }

    async fn assert_fresh_dials_are_unsent(
        state: &GatewayState,
        unix: &RelayClient,
        tcp: SocketAddr,
        list: Vec<u8>,
    ) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !state.listeners.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
            // The Unix client has already negotiated; its next admin call
            // must fail at dial, not after sending a request into the backlog.
            let control = RelayClient::call_control();
            let unix_error = unix
                .call(Endpoint::Admin, list, &control)
                .await
                .unwrap_err();
            let tcp = RelayClient::new(GatewayEndpoint::Tcp(tcp), None).unwrap();
            let tcp_error = tcp.version(&control).await.unwrap_err();
            for error in [unix_error, tcp_error] {
                let RelayCallError::Transport(failure) = error else {
                    panic!("expected a connect failure, got {error:?}");
                };
                assert_eq!(
                    failure.to_string(),
                    "HTTP transport failed in Connect (NotStarted, Io)"
                );
            }
        })
        .await
        .expect("fresh dials must fail while the accepted call is held");
    }

    #[test]
    fn join_timeout_keeps_holder_even_after_worker_finishes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("private/holder");
        let lock = Arc::new(HolderLock::try_acquire(&path, HolderMode::Exclusive).unwrap());
        let retained = Arc::downgrade(&lock);
        let (started, entered) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let (mut worker, accounts) = AccountWorker::start(
            Box::new(HeldCall {
                started,
                release: released,
            }),
            Arc::clone(&lock),
        )
        .unwrap();
        let call = std::thread::spawn(move || {
            worker.call(
                &crate::gateway::auth::Principal::LocalAdmin,
                None,
                AdminRequest::List {},
            )
        });
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        accounts.close();
        assert!(finish_accounts(accounts, lock, Instant::now()).is_err());
        let check_exclusive = || match HolderLock::try_acquire(&path, HolderMode::Shared) {
            Ok(_) => panic!("the exclusive holder was released"),
            Err(error) => assert!(holder::is_busy(&error), "{error}"),
        };
        check_exclusive();
        release.send(()).unwrap();
        assert!(matches!(
            call.join().unwrap(),
            AdminResponse::Accounts { .. }
        ));
        let deadline = Instant::now() + Duration::from_secs(2);
        while retained.strong_count() > 1 {
            assert!(
                Instant::now() < deadline,
                "worker did not drop its holder reference"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        check_exclusive();
    }
}
