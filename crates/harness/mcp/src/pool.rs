//! The process-wide MCP server pool: lazy start, leases, idle stop, and the
//! crash restart policy.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use plakous_model::{CancellationToken, Digest};
use plakous_tools::{
    ExecutionEnvironment, ExecutionOperationControl, McpServerName,
    SERVICE_PROCESS_TERMINATE_GRACE, ServiceHandle,
};
use rmcp::model::{PaginatedRequestParams, ProtocolVersion, Tool};
use rmcp::service::{ClientInitializeError, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientLifecycleMode, ClientServiceExt, Peer, RoleClient};
use tokio::sync::Semaphore;
use tokio::time::Instant;

use crate::client::{Handler, HandlerState};
use crate::error::{McpError, McpErrorKind};
use crate::http::{self, HttpRefusal, MAX_SSE_EVENT_BYTES, McpHttpEndpoint};
use crate::listing::{
    FrozenMcpListing, MAX_LISTED_TOOLS, MAX_LISTING_BYTES, McpToolSelection, freeze_listing,
    listed_tool_digest,
};
use crate::spec::{McpServerSpec, StdioServer, Transport};

/// Concurrent calls one pool entry dispatches; later calls queue.
pub const MAX_IN_FLIGHT_CALLS: usize = 16;
/// How long an entry with no lease keeps its server running.
pub const IDLE_STOP: Duration = Duration::from_secs(300);
/// A second crash within this window fails the entry for this long.
pub const CRASH_WINDOW: Duration = Duration::from_secs(60);

/// One process-wide pool of MCP server connections.
///
/// Entries are keyed by server id, definition digest and workspace root, start
/// at their first lease, are shared by every lease of the same key, and stop
/// [`IDLE_STOP`] after their last lease drops.
#[derive(Clone, Default)]
pub struct McpPool {
    inner: Arc<PoolInner>,
}

#[derive(Default)]
struct PoolInner {
    entries: Mutex<HashMap<EntryKey, Arc<Entry>>>,
    closed: std::sync::atomic::AtomicBool,
    /// The transport client HTTP servers derive theirs from; a default
    /// `WebPKI` client when the host supplied none.
    http: Option<plakous_http::Client>,
    runtime: Option<tokio::runtime::Handle>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct EntryKey {
    server: McpServerName,
    definition: Digest,
    root: PathBuf,
}

impl fmt::Debug for McpPool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpPool")
            .field("entries", &lock(&self.inner.entries).len())
            .finish()
    }
}

impl McpPool {
    /// Creates an empty pool.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty pool whose HTTP servers use `client`'s TLS
    /// configuration, observer and fault plan. Limits, pooling and the
    /// absence of an address policy are the pool's.
    #[must_use]
    pub fn with_http_client(client: plakous_http::Client) -> Self {
        Self {
            inner: Arc::new(PoolInner {
                http: Some(client),
                ..PoolInner::default()
            }),
        }
    }

    /// This pool with its connections and idle timers running on `runtime`,
    /// so a server outlives the executor of the session that started it.
    /// A builder step: apply it before the first lease.
    #[must_use]
    pub fn running_on(self, runtime: tokio::runtime::Handle) -> Self {
        Self {
            inner: Arc::new(PoolInner {
                http: self.inner.http.clone(),
                runtime: Some(runtime),
                ..PoolInner::default()
            }),
        }
    }

    /// Leases the entry for `spec` in `environment`'s workspace, starting its
    /// server if it is not running, and returns once its tools are listed.
    ///
    /// # Errors
    ///
    /// Returns a definition, startup or listing error, a failed-entry error
    /// inside the crash window, or a closed error after [`Self::shutdown`].
    pub async fn lease(
        &self,
        spec: McpServerSpec,
        environment: Arc<ExecutionEnvironment>,
    ) -> Result<McpLease, McpError> {
        if self.inner.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(closed());
        }
        let key = EntryKey {
            server: spec.name().clone(),
            definition: spec.definition_digest()?,
            root: environment.workspace().root().to_path_buf(),
        };
        let entry = {
            let mut entries = lock(&self.inner.entries);
            let entry = entries
                .entry(key.clone())
                .or_insert_with(|| {
                    Arc::new(Entry {
                        key,
                        spec,
                        environment,
                        http: self.inner.http.clone(),
                        runtime: self.inner.runtime.clone(),
                        pool: Arc::downgrade(&self.inner),
                        state: tokio::sync::Mutex::new(EntryState::Stopped),
                        leases: Mutex::new(Leases::default()),
                        permits: Semaphore::new(MAX_IN_FLIGHT_CALLS),
                        crash: Mutex::new(CrashRecord::default()),
                    })
                })
                .clone();
            let mut leases = lock(&entry.leases);
            leases.count += 1;
            leases.generation += 1;
            drop(leases);
            entry
        };
        let lease = McpLease { entry };
        lease.entry.connection().await?;
        Ok(lease)
    }

    /// Stops every entry and refuses later leases. Leases still held fail
    /// their calls with a closed error.
    pub async fn shutdown(&self) {
        self.inner
            .closed
            .store(true, std::sync::atomic::Ordering::Release);
        let entries = lock(&self.inner.entries)
            .drain()
            .map(|(_, entry)| entry)
            .collect::<Vec<_>>();
        for entry in entries {
            entry.stop(EntryState::Closed).await;
        }
    }
}

/// A session's hold on one pool entry; the entry idles once every lease
/// dropped.
pub struct McpLease {
    pub(crate) entry: Arc<Entry>,
}

impl fmt::Debug for McpLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpLease")
            .field("server", &self.entry.key.server)
            .finish_non_exhaustive()
    }
}

/// An entry's state for an operator view.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum McpServerState {
    /// The server is running.
    Running,
    /// The server is not running; the next call starts it.
    Stopped,
    /// The server crashed twice within [`CRASH_WINDOW`]; calls fail fast for
    /// this much longer.
    Failed {
        /// Time left in the fail-fast window.
        remaining: Duration,
    },
    /// The pool shut down.
    Closed,
}

/// Operator diagnostics of one entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpServerStatus {
    /// The entry's state.
    pub state: McpServerState,
    /// The retained stderr tail of the current or last server process. It is
    /// operator text, never model-visible.
    pub stderr_tail: String,
}

impl McpLease {
    /// Returns the server id.
    #[must_use]
    pub fn server(&self) -> &McpServerName {
        &self.entry.key.server
    }

    /// Returns the frozen definition.
    #[must_use]
    pub fn spec(&self) -> &McpServerSpec {
        &self.entry.spec
    }

    /// Freezes the template selection against the server's current listing.
    ///
    /// # Errors
    ///
    /// Returns the entry's start error, or the selection's refusal.
    pub async fn freeze(&self, selection: &McpToolSelection) -> Result<FrozenMcpListing, McpError> {
        let connection = self.entry.connection().await?;
        freeze_listing(&self.entry.spec, &connection.listing, selection)
    }

    /// Returns the name of every tool the server lists, in listing order,
    /// before the allowlist or any selection applies.
    ///
    /// # Errors
    ///
    /// Returns the entry's start error.
    pub async fn listed_tool_names(&self) -> Result<Vec<String>, McpError> {
        let connection = self.entry.connection().await?;
        Ok(connection
            .listing
            .iter()
            .map(|tool| tool.name.to_string())
            .collect())
    }

    /// Returns the entry's state and stderr tail.
    pub async fn status(&self) -> McpServerStatus {
        let state = self.entry.state.lock().await;
        let stderr_tail = match &*state {
            EntryState::Running(connection) => connection.stderr_tail(),
            _ => lock(&self.entry.crash).stderr_tail.clone(),
        };
        let state = match &*state {
            EntryState::Running(_) => McpServerState::Running,
            EntryState::Stopped => McpServerState::Stopped,
            EntryState::Closed => McpServerState::Closed,
            EntryState::Failed { until } => {
                let now = Instant::now();
                if *until > now {
                    McpServerState::Failed {
                        remaining: *until - now,
                    }
                } else {
                    McpServerState::Stopped
                }
            }
        };
        McpServerStatus { state, stderr_tail }
    }
}

impl Drop for McpLease {
    fn drop(&mut self) {
        let generation = {
            let mut leases = lock(&self.entry.leases);
            leases.count = leases.count.saturating_sub(1);
            if leases.count > 0 {
                return;
            }
            leases.generation
        };
        let Some(runtime) = self
            .entry
            .runtime
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok())
        else {
            return;
        };
        let entry = Arc::downgrade(&self.entry);
        let deadline = Instant::now() + IDLE_STOP;
        runtime.spawn(async move {
            tokio::time::sleep_until(deadline).await;
            let Some(entry) = entry.upgrade() else {
                return;
            };
            entry.idle_stop(generation).await;
        });
    }
}

#[derive(Default)]
struct Leases {
    count: usize,
    generation: u64,
}

#[derive(Default)]
struct CrashRecord {
    last: Option<Instant>,
    stderr_tail: String,
}

pub(crate) enum EntryState {
    Stopped,
    Running(Arc<Connection>),
    Failed { until: Instant },
    Closed,
}

pub(crate) struct Entry {
    key: EntryKey,
    spec: McpServerSpec,
    environment: Arc<ExecutionEnvironment>,
    http: Option<plakous_http::Client>,
    runtime: Option<tokio::runtime::Handle>,
    pool: Weak<PoolInner>,
    state: tokio::sync::Mutex<EntryState>,
    leases: Mutex<Leases>,
    pub(crate) permits: Semaphore,
    crash: Mutex<CrashRecord>,
}

impl Entry {
    pub(crate) const fn spec(&self) -> &McpServerSpec {
        &self.spec
    }

    /// The running connection, starting the server when it is stopped.
    pub(crate) async fn connection(&self) -> Result<Arc<Connection>, McpError> {
        let mut state = self.state.lock().await;
        match &*state {
            EntryState::Running(connection) if !connection.is_closed() => {
                return Ok(Arc::clone(connection));
            }
            EntryState::Running(connection) => {
                // The transport closed outside a call; that is a crash too.
                let connection = Arc::clone(connection);
                *state = self.record_crash(&connection);
                drop(state);
                connection.terminate().await;
                return Box::pin(self.connection()).await;
            }
            EntryState::Failed { until } if *until > Instant::now() => {
                let remaining = (*until - Instant::now()).as_secs().max(1);
                return Err(McpError::new(
                    McpErrorKind::Failed,
                    format!(
                        "mcp server {} crashed twice within {} s; calls fail for {remaining} s more",
                        self.key.server,
                        CRASH_WINDOW.as_secs()
                    ),
                ));
            }
            EntryState::Closed => return Err(closed()),
            EntryState::Stopped | EntryState::Failed { .. } => {}
        }
        let start = {
            let (spec, environment, http, root) = (
                self.spec.clone(),
                Arc::clone(&self.environment),
                self.http.clone(),
                self.key.root.clone(),
            );
            async move {
                match spec.transport() {
                    Transport::Stdio(stdio) => {
                        Connection::start_stdio(&spec, stdio, &environment, &root).await
                    }
                    Transport::Http(endpoint) => {
                        Connection::start_http(&spec, endpoint, http.as_ref(), &root).await
                    }
                }
            }
        };
        let connection = Arc::new(match &self.runtime {
            Some(runtime) => runtime.spawn(start).await.map_err(|error| {
                McpError::with_source(
                    McpErrorKind::Startup,
                    format!("mcp server {} start did not finish", self.key.server),
                    error,
                )
            })??,
            None => start.await?,
        });
        *state = EntryState::Running(Arc::clone(&connection));
        Ok(connection)
    }

    /// Records that `connection` died during a call and returns the state the
    /// entry moves to: stopped (restart on the next call) or failed.
    pub(crate) async fn crashed(&self, connection: &Arc<Connection>) {
        let mut state = self.state.lock().await;
        if let EntryState::Running(current) = &*state
            && Arc::ptr_eq(current, connection)
        {
            *state = self.record_crash(connection);
        }
        drop(state);
        connection.terminate().await;
    }

    fn record_crash(&self, connection: &Connection) -> EntryState {
        let now = Instant::now();
        let mut crash = lock(&self.crash);
        crash.stderr_tail = connection.stderr_tail();
        let repeated = crash
            .last
            .is_some_and(|last| now.duration_since(last) < CRASH_WINDOW);
        crash.last = Some(now);
        tracing::warn!(
            server = %self.key.server,
            repeated,
            "mcp server exited"
        );
        if repeated {
            EntryState::Failed {
                until: now + CRASH_WINDOW,
            }
        } else {
            EntryState::Stopped
        }
    }

    async fn idle_stop(self: Arc<Self>, generation: u64) {
        {
            let Some(pool) = self.pool.upgrade() else {
                return;
            };
            let mut entries = lock(&pool.entries);
            let leases = lock(&self.leases);
            if leases.count > 0 || leases.generation != generation {
                return;
            }
            drop(leases);
            if entries
                .get(&self.key)
                .is_some_and(|current| Arc::ptr_eq(current, &self))
            {
                entries.remove(&self.key);
            }
        }
        self.stop(EntryState::Stopped).await;
    }

    async fn stop(&self, next: EntryState) {
        let previous = std::mem::replace(&mut *self.state.lock().await, next);
        if let EntryState::Running(connection) = previous {
            connection.terminate().await;
        }
    }
}

/// One running server process and its rmcp client.
pub(crate) struct Connection {
    client: tokio::sync::Mutex<Option<RunningService<RoleClient, Handler>>>,
    pub(crate) peer: Peer<RoleClient>,
    pub(crate) handler: Arc<HandlerState>,
    process: Mutex<Option<ServiceHandle>>,
    stderr: Mutex<String>,
    listing: Arc<[Tool]>,
    pub(crate) digests: BTreeMap<String, Digest>,
}

impl Connection {
    async fn start_stdio(
        spec: &McpServerSpec,
        stdio: &StdioServer,
        environment: &ExecutionEnvironment,
        root: &std::path::Path,
    ) -> Result<Self, McpError> {
        let server = spec.name();
        let request = stdio.service_request(server)?;
        let started = within_startup(spec, async {
            let process = environment
                .spawn_service(
                    request,
                    ExecutionOperationControl::new(CancellationToken::new()),
                )
                .await
                .map_err(|error| {
                    McpError::with_source(
                        McpErrorKind::Startup,
                        format!("mcp server {server} could not be started"),
                        error,
                    )
                })?;
            let handle = process.handle;
            let handler = HandlerState::new(server.clone(), root);
            let client = match Handler(Arc::clone(&handler))
                .serve_with_lifecycle((process.stdout, process.stdin), lifecycle())
                .await
            {
                Ok(client) => client,
                Err(error) => {
                    let tail = handle.stderr_tail();
                    let _ = handle.terminate(SERVICE_PROCESS_TERMINATE_GRACE).await;
                    tracing::warn!(server = %server, stderr_tail = %tail, "mcp handshake failed");
                    return Err(handshake_failed(server, error));
                }
            };
            let listing = match list_tools(client.peer(), server).await {
                Ok(listing) => listing,
                Err(error) => {
                    let _ = handle.terminate(SERVICE_PROCESS_TERMINATE_GRACE).await;
                    return Err(error);
                }
            };
            Ok((client, handler, Some(handle), listing))
        })
        .await?;
        Self::new(server, started)
    }

    async fn start_http(
        spec: &McpServerSpec,
        endpoint: &McpHttpEndpoint,
        base: Option<&plakous_http::Client>,
        root: &std::path::Path,
    ) -> Result<Self, McpError> {
        let server = spec.name();
        let base = match base {
            Some(base) => base.clone(),
            None => {
                plakous_http::Client::new(plakous_http::Limits::default()).map_err(|error| {
                    McpError::with_source(
                        McpErrorKind::Startup,
                        format!("mcp server {server} transport could not be built"),
                        error,
                    )
                })?
            }
        };
        let client = endpoint.client(
            server,
            &base,
            spec.call_timeout().max(spec.startup_timeout()),
        )?;
        let config = StreamableHttpClientTransportConfig::with_uri(endpoint.url())
            .max_concurrent_requests(MAX_IN_FLIGHT_CALLS)
            .max_sse_event_size(MAX_SSE_EVENT_BYTES);
        let transport = StreamableHttpClientTransport::with_client(client, config);
        let started = within_startup(spec, async {
            let handler = HandlerState::new(server.clone(), root);
            let client = Handler(Arc::clone(&handler))
                .serve_with_lifecycle(transport, lifecycle())
                .await
                .map_err(|error| handshake_failed(server, error))?;
            let listing = list_tools(client.peer(), server).await?;
            Ok((client, handler, None, listing))
        })
        .await?;
        Self::new(server, started)
    }

    fn new(server: &McpServerName, started: Started) -> Result<Self, McpError> {
        let (client, handler, handle, listing) = started;
        let digests = listing
            .iter()
            .map(|tool| Ok((tool.name.to_string(), listed_tool_digest(server, tool)?)))
            .collect::<Result<BTreeMap<_, _>, McpError>>()?;
        Ok(Self {
            peer: client.peer().clone(),
            client: tokio::sync::Mutex::new(Some(client)),
            handler,
            process: Mutex::new(handle),
            stderr: Mutex::new(String::new()),
            listing: listing.into(),
            digests,
        })
    }

    fn is_closed(&self) -> bool {
        self.peer.is_transport_closed()
    }

    fn stderr_tail(&self) -> String {
        lock(&self.process)
            .as_ref()
            .map_or_else(|| lock(&self.stderr).clone(), ServiceHandle::stderr_tail)
    }

    /// Closes the client and terminates the process group.
    pub(crate) async fn terminate(&self) {
        if let Some(mut client) = self.client.lock().await.take() {
            let _ = client
                .close_with_timeout(SERVICE_PROCESS_TERMINATE_GRACE)
                .await;
        }
        let handle = lock(&self.process).take();
        if let Some(handle) = handle {
            *lock(&self.stderr) = handle.stderr_tail();
            if let Err(error) = handle.terminate(SERVICE_PROCESS_TERMINATE_GRACE).await {
                tracing::warn!(%error, "mcp server process group was not joined");
            }
        }
    }
}

type Started = (
    RunningService<RoleClient, Handler>,
    Arc<HandlerState>,
    Option<ServiceHandle>,
    Vec<Tool>,
);

/// Newest first: a server that rejects the newest revision names what it
/// supports, and discovery retries with that.
fn lifecycle() -> ClientLifecycleMode {
    ClientLifecycleMode::Auto {
        preferred_versions: vec![
            ProtocolVersion::V_2026_07_28,
            ProtocolVersion::V_2025_11_25,
            ProtocolVersion::V_2025_06_18,
            ProtocolVersion::V_2025_03_26,
            ProtocolVersion::V_2024_11_05,
        ],
        legacy_version: Some(ProtocolVersion::LATEST_WITH_INITIALIZE),
    }
}

/// Bounds spawn, handshake and the first listing by the startup timeout.
async fn within_startup(
    spec: &McpServerSpec,
    start: impl Future<Output = Result<Started, McpError>>,
) -> Result<Started, McpError> {
    let startup = spec.startup_timeout();
    tokio::time::timeout(startup, start).await.map_err(|_| {
        McpError::new(
            McpErrorKind::Startup,
            format!(
                "mcp server {} did not start and list its tools within {} s",
                spec.name(),
                startup.as_secs()
            ),
        )
    })?
}

fn handshake_failed(server: &McpServerName, error: ClientInitializeError) -> McpError {
    let message = match http::classify(&error) {
        Some(HttpRefusal::Credentials(status)) => http::credentials_text(server, status),
        _ => format!("mcp server {server} handshake failed"),
    };
    McpError::with_source(McpErrorKind::Startup, message, error)
}

/// Pages through `tools/list` within [`MAX_LISTED_TOOLS`] and
/// [`MAX_LISTING_BYTES`].
async fn list_tools(
    peer: &Peer<RoleClient>,
    server: &McpServerName,
) -> Result<Vec<Tool>, McpError> {
    let mut tools = Vec::new();
    let mut bytes = 0_usize;
    let mut cursor = None;
    loop {
        let page = peer
            .list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor)))
            .await
            .map_err(|error| {
                McpError::with_source(
                    McpErrorKind::Listing,
                    format!("mcp server {server} tools/list failed"),
                    error,
                )
            })?;
        for tool in page.tools {
            bytes = bytes.saturating_add(
                serde_json::to_vec(&tool).map_or(usize::MAX, |encoded| encoded.len()),
            );
            tools.push(tool);
            if tools.len() > MAX_LISTED_TOOLS || bytes > MAX_LISTING_BYTES {
                return Err(McpError::new(
                    McpErrorKind::Listing,
                    format!(
                        "mcp server {server} lists more than {MAX_LISTED_TOOLS} tools or {} MiB",
                        MAX_LISTING_BYTES / (1024 * 1024)
                    ),
                ));
            }
        }
        match page.next_cursor {
            Some(next) if !next.is_empty() => cursor = Some(next),
            _ => return Ok(tools),
        }
    }
}

fn closed() -> McpError {
    McpError::new(McpErrorKind::Closed, "the mcp pool is shut down")
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
