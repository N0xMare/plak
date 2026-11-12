//! The runner end: answers host connections from a local execution
//! environment, with the owner and op tables, the per-line process sessions
//! and the services of this instance.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use plakous_model::{AttemptId, BranchId, CallId, CancellationToken, LineId, Timestamp};
use plakous_runner_protocol::frame::FrameError;
use plakous_runner_protocol::{
    Bind, Hello, HostFrame, LineOwner, LineStatus, Refusal, Request, RequestClass, RunnerFrame,
    StreamFrame, VersionTriple, WireError, capabilities,
};
use plakous_tools::internal::runner::{
    RunnerExtension, RunnerSecretResolver, RunnerVerb, RunnerVerbFamily, ServeFailure, ServeFuture,
    decode_preempt, environment_failure, job_capacity, line_sessions, requested_workspace,
    runner_descriptor, serve, serve_failure, serve_jobs_watch, serve_probe, serve_service_spawn,
    serve_service_terminate, serve_sessions, service_spawned, terminated_service,
};
use plakous_tools::{
    ExecutionEnvironment, ExecutionEnvironmentError, JoinedProcessHostCapacity,
    JoinedProcessSessions, OperationClass, OperationContext, RunnerId, ServiceHandle,
    ServiceProcess, ToolPreempt,
};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, watch};

use crate::connection::{FrameStream, fits, write_prioritized};
use crate::floors::OwnerFloors;
use crate::state::{Admission, OpKey, Tables};
use crate::streams::{
    Credit, INITIAL_CREDIT, Outbound, StreamEnds, Window, receive_stream, send_stream,
};

mod foreground;
mod operator;
pub(crate) mod qualification;

/// One runner process: its identity, its environment and the state every
/// connection to it shares. A new service is a new instance.
#[derive(Clone)]
pub struct RunnerService {
    inner: Arc<Inner>,
}

struct Inner {
    operator: Option<crate::operator::OperatorTerminalService>,
    environment: ExecutionEnvironment,
    changes_git_enabled: bool,
    workspaces: Option<Arc<WorkspaceFactory>>,
    /// Environments registered for workspaces other than the default one,
    /// by root and cwd as requests name them.
    registered: Mutex<HashMap<(PathBuf, PathBuf), ExecutionEnvironment>>,
    extension: Option<Arc<dyn RunnerExtension>>,
    secrets: Option<Arc<RunnerSecretResolver>>,
    hello: Hello,
    dead: Duration,
    owner_lease: Duration,
    lease_clock: Option<Arc<RunnerLeaseClock>>,
    capacity: JoinedProcessHostCapacity,
    tables: Mutex<Tables>,
    /// Which hosts may bind; `None` admits every host.
    hosts_allowed: Option<Arc<HostAllowlist>>,
    /// Which paths a probe answers about; `None` admits every path.
    probe_paths: Option<Arc<PathAdmission>>,
    /// The host identity of this machine's own product home: an inspection
    /// under it is a local `plak runner ps` and sees every host's lines.
    local_host: Option<String>,
    /// See [`RunnerOptions::home_local`].
    home_local: Option<String>,
    /// Each host instance's current connection: a key's answer goes to the
    /// instance that last sent the key, so an answer that outlives its
    /// connection reaches that instance's reconnection.
    hosts: Mutex<HashMap<(String, String), Connection>>,
    /// Each host instance's owner lease: its connections, and since when it
    /// has none. An expired lease is removed.
    leases: Mutex<HashMap<(String, String), HostLease>>,
    foreground: foreground::Authority,
    workspace_catalog: Option<Arc<crate::WorkspaceCatalog>>,
    next_connection: AtomicU64,
    /// Connections open now, bound or not.
    open: AtomicU64,
    closing: AtomicBool,
    next_stream: AtomicU64,
    /// Each running request's cancellation and yield, by its op key when it
    /// is keyed, so a replay that moved the key's request id still reaches
    /// it.
    running: Mutex<HashMap<(String, RunKey), Running>>,
    /// Each line's process sessions, per host. They outlive connections and
    /// owner changes: a newer owner of the line finds its jobs here.
    lines: Mutex<HashMap<(String, String), (JoinedProcessSessions, ExecutionEnvironment)>>,
    /// Each service, per host and stream. A service lives as long as the
    /// connection that spawned it.
    services: Mutex<HashMap<(String, u64), Hosted>>,
}

struct Connection {
    sender: mpsc::UnboundedSender<RunnerFrame>,
    closer: CancellationToken,
    id: u64,
}

struct Running {
    instance: String,
    cancellation: CancellationToken,
    preempt: watch::Sender<ToolPreempt>,
}

/// One host instance's owner lease. The lines it holds are held while it has
/// a connection and for the lease after the last one ends; then their jobs
/// keep running as orphans until the same instance returns or a newer owner
/// of the same host takes each line over. Other instances of the host
/// neither extend nor end it.
#[derive(Default)]
struct HostLease {
    connections: usize,
    // Monotonic lease time excludes suspend on both supported platforms.
    released: Option<Instant>,
}

struct Hosted {
    ends: StreamEnds,
    handle: Option<ServiceHandle>,
    connection: u64,
}

/// What a runner says about itself in its hello.
#[derive(Clone, Debug)]
pub struct RunnerIdentity {
    /// The runner's stable identity; never `local`.
    pub runner_id: RunnerId,
    /// The version triple it binds.
    pub version: VersionTriple,
    /// The machine boot it runs in.
    pub boot_id: String,
    /// Its build's binary id, as evidence.
    pub binary_id: Option<String>,
}

/// A runner's bounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunnerLimits {
    /// How many processes run at once across every line and host (128).
    pub job_bound: u32,
    /// How long a connection may stay silent before the runner ends it
    /// (15 s; the host pings every 5 s).
    pub dead: Duration,
    /// How long a host's lines stay held after its last connection ends
    /// (300 s): its jobs keep running either way, and are orphans after.
    pub owner_lease: Duration,
}

impl Default for RunnerLimits {
    fn default() -> Self {
        Self {
            job_bound: 128,
            dead: Duration::from_secs(15),
            owner_lease: Duration::from_secs(300),
        }
    }
}

/// Answers whether a host identity may bind, asked at every bind so a
/// pairing made while the runner runs takes effect at once.
pub type HostAllowlist = dyn Fn(&str) -> bool + Send + Sync;

/// Answers whether a probe may answer about a path.
pub type PathAdmission = dyn Fn(&Path) -> bool + Send + Sync;

/// Suspend-excluding, nondecreasing monotonic lease time. Embedders can supply
/// a controlled clock for authority-loss tests; production defaults to `Instant`.
pub type RunnerLeaseClock = dyn Fn() -> Instant + Send + Sync;

/// Makes the environment of a workspace a request names, by root and cwd,
/// the first time one does: a runner serves every workspace it can open.
pub type WorkspaceFactory =
    dyn Fn(&Path, &Path) -> Result<ExecutionEnvironment, ExecutionEnvironmentError> + Send + Sync;

/// What a runner serves beyond its default environment.
#[derive(Clone, Default)]
pub struct RunnerOptions {
    /// The embedding build's Git CLI gate. Product callers pass the existing
    /// compile-time gate, never a client or environment value. Defaults closed.
    pub changes_git_enabled: bool,
    /// Operator-only PTY workspace policy; absent means no operator capability.
    pub operator_workspace: Option<Arc<crate::operator::OperatorWorkspace>>,
    /// Explicit lease clock; absent uses the platform's suspend-excluding clock.
    pub lease_clock: Option<Arc<RunnerLeaseClock>>,
    /// Stable physical-machine identity; unknown identities cannot be paired for placement.
    pub machine_id: Option<String>,
    /// Runner-owned effective workspace consent and bounded discovery.
    pub workspace_catalog: Option<Arc<crate::WorkspaceCatalog>>,
    /// Its bounds.
    pub limits: RunnerLimits,
    /// Environments for other workspaces; without it, a request naming
    /// another workspace is refused.
    pub workspaces: Option<Arc<WorkspaceFactory>>,
    /// The verbs the embedding program answers.
    pub extension: Option<Arc<dyn RunnerExtension>>,
    /// Which hosts may bind; without it every host may.
    pub hosts: Option<Arc<HostAllowlist>>,
    /// Which paths a workspace probe answers about; without it every path.
    pub probe_paths: Option<Arc<PathAdmission>>,
    /// The host identity of the runner machine's own product home. An
    /// inspecting connection under it sees every host's lines in full;
    /// any other sees only its own host's, with job commands and output
    /// redacted.
    pub local_host: Option<String>,
    /// Where accepted owners are recorded; without it a restart forgets
    /// them.
    pub floors: Option<Arc<dyn OwnerFloors>>,
    /// The `PATH` and `HOME` the runner's processes start with, announced
    /// in its hello.
    pub path: Option<String>,
    /// See [`Self::path`].
    pub home: Option<String>,
    /// The runner's own secret store, which a service's named runner
    /// secrets are read from; without it a service that names one is
    /// refused.
    pub secrets: Option<Arc<RunnerSecretResolver>>,
    /// Set, a home-local runner of the home whose host identity it names
    /// (spec 01 §8.4): it binds only that host, whatever [`Self::hosts`]
    /// says, from a build of its protocol version whatever the build's
    /// revision or flavor; it answers only the process-session verbs; and it
    /// starts each process in the process context its host sends with the
    /// start.
    pub home_local: Option<String>,
}

impl RunnerService {
    /// A new runner instance answering from `environment`, with the default
    /// limits.
    ///
    /// # Errors
    ///
    /// Refuses the reserved `local` runner id.
    pub fn new(
        environment: ExecutionEnvironment,
        identity: RunnerIdentity,
    ) -> Result<Self, ExecutionEnvironmentError> {
        Self::with_limits(environment, identity, RunnerLimits::default())
    }

    /// A new runner instance answering from `environment` within `limits`.
    ///
    /// # Errors
    ///
    /// Refuses the reserved `local` runner id.
    pub fn with_limits(
        environment: ExecutionEnvironment,
        identity: RunnerIdentity,
        limits: RunnerLimits,
    ) -> Result<Self, ExecutionEnvironmentError> {
        Self::with_options(
            environment,
            identity,
            RunnerOptions {
                limits,
                ..RunnerOptions::default()
            },
        )
    }

    /// A new runner instance answering from `environment` with `options`.
    ///
    /// # Errors
    ///
    /// Refuses the reserved `local` runner id.
    pub fn with_options(
        environment: ExecutionEnvironment,
        identity: RunnerIdentity,
        options: RunnerOptions,
    ) -> Result<Self, ExecutionEnvironmentError> {
        let limits = options.limits;
        let descriptor = runner_descriptor(&environment, identity.runner_id.clone())?;
        let mut hello = Hello {
            version: identity.version,
            binary_id: identity.binary_id,
            runner_id: identity.runner_id.as_str().to_owned(),
            instance: uuid::Uuid::new_v4().to_string(),
            boot_id: identity.boot_id,
            machine_id: options.machine_id,
            descriptor,
            capabilities: vec![
                capabilities::WORKSPACE_VERBS.to_owned(),
                capabilities::STREAMS.to_owned(),
            ],
            path: options.path,
            home: options.home,
            job_bound: limits.job_bound,
        };
        let tables = options
            .floors
            .map_or_else(Tables::default, Tables::with_floors);
        let operator = options.operator_workspace.and_then(|workspace| {
            crate::operator::OperatorTerminalService::new(
                hello.instance.clone(),
                workspace,
                options.lease_clock.clone(),
            )
            .ok()
        });
        if operator.is_some() {
            hello.capabilities.push(capabilities::OPERATOR_PTY.into());
        }
        Ok(Self {
            inner: Arc::new(Inner {
                operator,
                environment,
                changes_git_enabled: options.changes_git_enabled,
                workspaces: options.workspaces,
                registered: Mutex::default(),
                extension: options.extension,
                secrets: options.secrets,
                hello,
                dead: limits.dead,
                owner_lease: limits.owner_lease,
                lease_clock: options.lease_clock,
                capacity: job_capacity(usize::try_from(limits.job_bound).unwrap_or(usize::MAX)),
                tables: Mutex::new(tables),
                hosts_allowed: options.hosts,
                probe_paths: options.probe_paths,
                local_host: options.local_host,
                home_local: options.home_local,
                hosts: Mutex::default(),
                leases: Mutex::default(),
                foreground: foreground::Authority::default(),
                workspace_catalog: options.workspace_catalog,
                next_connection: AtomicU64::new(0),
                open: AtomicU64::new(0),
                closing: AtomicBool::new(false),
                next_stream: AtomicU64::new(0),
                running: Mutex::default(),
                lines: Mutex::default(),
                services: Mutex::default(),
            }),
        })
    }

    /// What this instance says first on every connection.
    #[must_use]
    pub fn hello(&self) -> &Hello {
        &self.inner.hello
    }

    /// Answers one connection until it ends, it stays silent for the dead
    /// window, or a newer connection of the same host instance binds.
    /// Requests it admitted keep running after that and answer on the
    /// instance's next connection; jobs keep running; its services end.
    pub async fn serve_connection<S>(&self, stream: S)
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        self.inner.open.fetch_add(1, Ordering::AcqRel);
        self.serve_open_connection(stream).await;
        self.inner.open.fetch_sub(1, Ordering::AcqRel);
    }

    async fn serve_open_connection<S>(&self, stream: S)
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (reader, writer) = tokio::io::split(stream);
        let (sender, frames) = mpsc::unbounded_channel();
        let (terminal_sender, terminal_frames) = mpsc::channel(64);
        let closer = CancellationToken::new();
        tokio::spawn(write_prioritized(
            writer,
            frames,
            terminal_frames,
            closer.clone(),
        ));
        let mut reader = FrameStream::new(reader);
        let _ = sender.send(RunnerFrame::Hello(self.inner.hello.clone()));
        let bind = tokio::time::timeout(self.inner.dead, reader.next::<HostFrame>()).await;
        let bind = match bind {
            Ok(Some(Ok(HostFrame::Bind(bind)))) => bind,
            Ok(Some(Ok(HostFrame::Qualify { host_id, version }))) => {
                self.serve_qualification(&host_id, &version, &mut reader, &sender, &closer)
                    .await;
                closer.cancel();
                return;
            }
            _ => {
                let _ = sender.send(refused(None, Refusal::Unbound));
                closer.cancel();
                return;
            }
        };
        if let Some(refusal) = self.bind_refusal(&bind) {
            let _ = sender.send(refused(None, refusal));
            closer.cancel();
            return;
        }
        if bind.inspect {
            let everyone = self.inner.local_host.as_deref() == Some(bind.host_id.as_str());
            self.serve_inspection(
                &mut reader,
                &sender,
                &closer,
                (!everyone).then_some(&bind.host_id),
            )
            .await;
            closer.cancel();
            return;
        }
        if self
            .inner
            .hosts_allowed
            .as_ref()
            .is_some_and(|allowed| !allowed(&bind.host_id))
        {
            let _ = sender.send(refused(
                None,
                Refusal::NotPaired {
                    host_id: bind.host_id,
                },
            ));
            closer.cancel();
            return;
        }
        let Bind {
            host_id: host,
            host_instance: instance,
            ..
        } = bind;
        self.serve_bound_connection(
            &host,
            &instance,
            &mut reader,
            sender,
            terminal_sender,
            closer,
        )
        .await;
    }

    async fn serve_bound_connection<R: AsyncRead + Unpin>(
        &self,
        host: &str,
        instance: &str,
        reader: &mut FrameStream<R>,
        sender: mpsc::UnboundedSender<RunnerFrame>,
        terminal_sender: mpsc::Sender<RunnerFrame>,
        closer: CancellationToken,
    ) {
        let connection = self.enter(host, instance, &sender, &closer);
        let mut terminal_tick = tokio::time::interval(Duration::from_millis(64));
        let mut read_deadline = tokio::time::Instant::now() + self.inner.dead;
        loop {
            let reserve = self.inner.operator.as_ref().map_or(1, |operator| {
                operator.control_reserve(host, instance, connection)
            });
            let readable = terminal_sender.capacity() >= reserve;
            let frame = tokio::select! {
                frame = reader.next::<HostFrame>(), if readable => frame,
                () = tokio::time::sleep_until(read_deadline) => break,
                room = terminal_sender.reserve_many(reserve), if !readable => {
                    if room.is_err() { break; }
                    continue;
                }
                () = closer.cancelled() => break,
                _ = terminal_tick.tick(), if self.inner.operator.is_some() => {
                    self.operator_output(host, instance, connection, &terminal_sender);
                    continue;
                }
            };
            let frame = match frame {
                None => break,
                Some(Ok(frame)) => frame,
                Some(Err(error)) => {
                    let _ = sender.send(refused(
                        None,
                        Refusal::Malformed {
                            detail: error.to_string(),
                        },
                    ));
                    // An oversized prefix leaves the stream unframed: the
                    // connection ends.
                    if error == FrameError::TooLarge {
                        break;
                    }
                    continue;
                }
            };
            read_deadline = tokio::time::Instant::now() + self.inner.dead;
            self.receive(host, instance, connection, frame, &sender, &terminal_sender);
        }
        self.leave(host, instance, connection);
        closer.cancel();
    }

    /// Why `bind` is refused before anything else: another version, or, on a
    /// home-local runner, another protocol version or another home's host,
    /// which it serves not even to inspect.
    fn bind_refusal(&self, bind: &Bind) -> Option<Refusal> {
        let ours = &self.inner.hello.version;
        let version_bound = match &self.inner.home_local {
            Some(_) => bind.version.protocol == ours.protocol,
            None => bind.version == *ours,
        };
        if !version_bound {
            return Some(Refusal::VersionMismatch {
                runner: ours.clone(),
            });
        }
        match &self.inner.home_local {
            Some(home) if *home != bind.host_id => Some(Refusal::NotPaired {
                host_id: bind.host_id.clone(),
            }),
            _ => None,
        }
    }

    /// Makes a bound connection its host instance's current one, draining
    /// the older one, and holds that instance's owner lease while it lives.
    fn enter(
        &self,
        host: &str,
        instance: &str,
        sender: &mpsc::UnboundedSender<RunnerFrame>,
        closer: &CancellationToken,
    ) -> u64 {
        // A late reconnect cannot revive authority before the idle tick runs.
        self.expire_leases(self.lease_now());
        let connection = self.inner.next_connection.fetch_add(1, Ordering::Relaxed);
        let previous = lock(&self.inner.hosts).insert(
            (host.to_owned(), instance.to_owned()),
            Connection {
                sender: sender.clone(),
                closer: closer.clone(),
                id: connection,
            },
        );
        // A host instance binds again only once its older connection is
        // dead to it: that connection is drained, and its answers come here.
        if let Some(previous) = previous {
            previous.closer.cancel();
        }
        let mut leases = lock(&self.inner.leases);
        let lease = leases
            .entry((host.to_owned(), instance.to_owned()))
            .or_default();
        lease.connections += 1;
        lease.released = None;
        connection
    }

    /// The connection is over: answers no longer go to it, its services end,
    /// and its instance's owner lease starts running out with its last one.
    fn leave(&self, host: &str, instance: &str, connection: u64) {
        {
            let mut hosts = lock(&self.inner.hosts);
            let key = (host.to_owned(), instance.to_owned());
            if hosts
                .get(&key)
                .is_some_and(|current| current.id == connection)
            {
                hosts.remove(&key);
            }
        }
        {
            let mut leases = lock(&self.inner.leases);
            if let Some(lease) = leases.get_mut(&(host.to_owned(), instance.to_owned())) {
                lease.connections = lease.connections.saturating_sub(1);
                if lease.connections == 0 {
                    lease.released = Some(self.lease_now());
                }
            }
        }
        self.end_services(host, connection);
        if let Some(operator) = &self.inner.operator {
            operator.disconnect(host, connection);
        }
    }

    /// An inspecting connection: line-table questions and pings until it
    /// ends; anything else is refused and ends it. Scoped to `host`, it sees
    /// only that host's lines, without job commands or output; unscoped, a
    /// local `plak runner ps`, every host's.
    async fn serve_inspection<R: AsyncRead + Unpin>(
        &self,
        reader: &mut FrameStream<R>,
        sender: &mpsc::UnboundedSender<RunnerFrame>,
        closer: &CancellationToken,
        host: Option<&String>,
    ) {
        loop {
            let frame = tokio::select! {
                frame = tokio::time::timeout(self.inner.dead, reader.next::<HostFrame>()) => frame,
                () = closer.cancelled() => return,
            };
            match frame {
                Ok(Some(Ok(HostFrame::Ping { nonce }))) => {
                    let _ = sender.send(RunnerFrame::Pong { nonce });
                }
                Ok(Some(Ok(HostFrame::Lines { request_id }))) => {
                    let mut lines = self.lines(host.map(String::as_str)).await;
                    if host.is_some() {
                        lines.iter_mut().for_each(redact);
                    }
                    let _ = sender.send(RunnerFrame::Lines { request_id, lines });
                }
                Ok(Some(Ok(_))) => {
                    let _ = sender.send(refused(None, Refusal::InspectOnly));
                    return;
                }
                Ok(Some(Err(_)) | None) | Err(_) => return,
            }
        }
    }

    /// Ends the owner lease of every host instance whose last connection
    /// ended more than the lease ago: the lines it holds are no longer held,
    /// and their jobs keep running as orphans until it returns or a newer
    /// owner of the same host takes each line over.
    fn expire_leases(&self, now: Instant) {
        let expired = {
            let leases = lock(&self.inner.leases);
            leases
                .iter()
                .filter(|(_, lease)| {
                    lease.connections == 0
                        && lease
                            .released
                            .is_some_and(|at| now.duration_since(at) >= self.inner.owner_lease)
                })
                .filter_map(|(holder, lease)| {
                    lease.released.map(|released| (holder.clone(), released))
                })
                .collect::<Vec<_>>()
        };
        if expired.is_empty() {
            return;
        }
        for ((host, instance), released) in expired {
            self.expire_foreground(host, instance, released);
        }
    }

    fn lease_now(&self) -> Instant {
        self.inner
            .lease_clock
            .as_ref()
            .map_or_else(Instant::now, |clock| clock())
    }

    /// The line table: every line of `host` that holds sessions here, or of
    /// every host, with its owner, whether that owner holds it, and its jobs.
    pub(crate) async fn lines(&self, host: Option<&str>) -> Vec<LineStatus> {
        self.expire_leases(self.lease_now());
        let mut held: Vec<(
            (String, String),
            (JoinedProcessSessions, ExecutionEnvironment),
        )> = lock(&self.inner.lines)
            .iter()
            .filter(|((line_host, _), _)| host.is_none_or(|host| host == line_host))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        held.sort_by(|left, right| left.0.cmp(&right.0));
        let mut lines = Vec::with_capacity(held.len());
        for ((line_host, line), (sessions, environment)) in held {
            let jobs = serve_sessions(
                &environment,
                &sessions,
                &self.inner.hello.instance,
                RunnerVerb::JobsSnapshot.as_str(),
                Value::Null,
                OperationContext::new(CancellationToken::new()),
                self.inner.home_local.is_some(),
            )
            .await
            .unwrap_or(Value::Null);
            let owned = lock(&self.inner.tables).owner(&line_host, &line);
            lines.push(LineStatus {
                adopted: owned.as_ref().is_some_and(|(_, adopted)| *adopted),
                owner: owned.map(|(owner, _)| owner),
                host_id: line_host,
                line,
                jobs,
                foreground: sessions.active_session_ids().into_iter().any(|id| {
                    !sessions.is_promoted(id) && sessions.joined_terminal_status(id).is_none()
                }),
            });
        }
        lines
    }

    /// Whether the runner may exit: no connection, no service, no job
    /// running or orphaned on any line, and no answer or tombstone a host
    /// has not acknowledged (spec 01 §8.4).
    #[must_use]
    pub fn is_idle(&self) -> bool {
        let now = Instant::now();
        self.expire_leases(self.lease_now());
        self.inner.foreground.prune();
        if self
            .inner
            .operator
            .as_ref()
            .is_some_and(crate::operator::OperatorTerminalService::has_live_terminals)
        {
            return false;
        }
        if self.inner.open.load(Ordering::Acquire) != 0 || !lock(&self.inner.services).is_empty() {
            return false;
        }
        let running = lock(&self.inner.lines).values().any(|(sessions, _)| {
            sessions
                .active_session_ids()
                .into_iter()
                .any(|id| sessions.joined_terminal_status(id).is_none())
        });
        !running
            && !self.inner.foreground.pending()
            && !lock(&self.inner.tables).holds_unacknowledged(now)
    }

    fn receive(
        &self,
        host: &str,
        instance: &str,
        connection: u64,
        frame: HostFrame,
        sender: &mpsc::UnboundedSender<RunnerFrame>,
        terminal_sender: &mpsc::Sender<RunnerFrame>,
    ) {
        match frame {
            HostFrame::Request(request) => {
                if matches!(request.class, RequestClass::OperatorControl { .. }) {
                    self.operator_request(host, instance, connection, request, sender);
                } else {
                    self.admit(host, instance, connection, request, sender);
                }
            }
            HostFrame::Cancel { request_id } => {
                let run = self.run_key(host, instance, request_id);
                if let Some(running) = lock(&self.inner.running).get(&(host.to_owned(), run))
                    && running.instance == instance
                {
                    running.cancellation.cancel();
                }
            }
            HostFrame::Preempt { request_id, value } => {
                let run = self.run_key(host, instance, request_id);
                if let Some(running) = lock(&self.inner.running).get(&(host.to_owned(), run))
                    && running.instance == instance
                {
                    let _ = running.preempt.send(decode_preempt(&value));
                }
            }
            HostFrame::Ack { request_id } => {
                self.inner
                    .foreground
                    .acknowledge(host, instance, &request_id);
                lock(&self.inner.tables).acknowledge(host, instance, &request_id);
            }
            HostFrame::TransferOwner(transfer) => {
                let service = self.clone();
                let host = host.to_owned();
                let instance = instance.to_owned();
                let sender = sender.clone();
                tokio::spawn(async move {
                    let answer = service
                        .transfer_foreground(&host, &instance, transfer)
                        .await;
                    let _ = sender.send(answer);
                });
            }
            HostFrame::Lines { request_id } => {
                let service = self.clone();
                let host = host.to_owned();
                let sender = sender.clone();
                tokio::spawn(async move {
                    let lines = service.lines(Some(&host)).await;
                    let _ = sender.send(RunnerFrame::Lines { request_id, lines });
                });
            }
            HostFrame::Ping { nonce } => {
                let _ = sender.send(RunnerFrame::Pong { nonce });
            }
            HostFrame::Bind(_) | HostFrame::Qualify { .. } => {
                let _ = sender.send(refused(
                    None,
                    Refusal::Malformed {
                        detail: "the connection is already bound".into(),
                    },
                ));
            }
            HostFrame::Stream(frame) => {
                if !self.operator_link_current(host, instance, connection) {
                    return;
                }
                if let Some(operator) = &self.inner.operator
                    && let Some(frames) =
                        operator.receive(host, instance, connection, frame.clone())
                {
                    for frame in frames {
                        let _ = terminal_sender.try_send(RunnerFrame::Stream(frame));
                    }
                } else {
                    self.stream(host, frame);
                }
            }
        }
    }

    fn admit(
        &self,
        host: &str,
        instance: &str,
        connection: u64,
        request: Request,
        sender: &mpsc::UnboundedSender<RunnerFrame>,
    ) {
        let id = request.request_id.clone();
        let Some(verb) = RunnerVerb::parse(&request.operation.verb) else {
            let _ = sender.send(refused(
                Some(id),
                Refusal::Unsupported {
                    what: request.operation.verb,
                },
            ));
            return;
        };
        // Home-local is process-only. Changes is operator-only and must never
        // reserve a model/effect key or carry an owner.
        if (self.inner.home_local.is_some() && verb.family() != RunnerVerbFamily::Sessions)
            || (verb == RunnerVerb::WorkspaceChanges
                && (request.class != RequestClass::BuildRead || request.owner.is_some()))
        {
            let _ = sender.send(refused(
                Some(id),
                Refusal::Unsupported {
                    what: request.operation.verb,
                },
            ));
            return;
        }
        if verb.family() == RunnerVerbFamily::Sessions && request.owner.is_none() {
            let _ = sender.send(refused(
                Some(id),
                Refusal::Malformed {
                    detail: "a process-session request names its owner".into(),
                },
            ));
            return;
        }
        let cancellation = CancellationToken::new();
        let (preempt, yields) = watch::channel(ToolPreempt::None);
        let control = match context(&request, cancellation.clone(), yields) {
            Ok(control) => control,
            Err(detail) => {
                let _ = sender.send(refused(Some(id), Refusal::Malformed { detail }));
                return;
            }
        };
        let key = match lock(&self.inner.tables).admit(host, instance, &request, Instant::now()) {
            Admission::Answer(frame) => {
                let _ = sender.send(frame);
                return;
            }
            Admission::Run(key) => key,
        };
        let run = key
            .clone()
            .map_or_else(|| RunKey::Request(id.clone()), RunKey::Op);
        lock(&self.inner.running).insert(
            (host.to_owned(), run),
            Running {
                instance: instance.to_owned(),
                cancellation,
                preempt,
            },
        );
        let service = self.clone();
        let host = host.to_owned();
        let instance = instance.to_owned();
        let payload = request.operation.payload;
        let served = match verb.family() {
            RunnerVerbFamily::Workspace => {
                crate::workspace::serve(self.inner.workspace_catalog.clone(), verb, payload)
            }
            RunnerVerbFamily::Environment => self.serve_environment(verb, payload, control),
            RunnerVerbFamily::Probe => match &self.inner.probe_paths {
                Some(admits) => serve_probe(payload, admits.as_ref()),
                None => serve_probe(payload, &|_| true),
            },
            RunnerVerbFamily::Extension => self.serve_extension(verb, payload),
            RunnerVerbFamily::Sessions if verb == RunnerVerb::JobsWatch => {
                let line = request.owner.map(|owner| owner.line).unwrap_or_default();
                self.watch_jobs(&host, connection, &line, &payload)
            }
            RunnerVerbFamily::Sessions => {
                self.fenced_sessions(&host, &instance, request.owner, verb, payload, control)
            }
            RunnerVerbFamily::Service if verb == RunnerVerb::ServiceSpawn => {
                self.spawn_service(&host, connection, payload, control)
            }
            RunnerVerbFamily::Service => self.terminate_service(&host, payload),
        };
        tokio::spawn(async move {
            let answer = answer_frame(id.clone(), served.await);
            service.finish(&host, &instance, &id, key.as_ref(), answer);
        });
    }

    /// Records a request's answer and sends it to the host instance waiting
    /// for it: for a key, the instance that last sent it.
    fn finish(
        &self,
        host: &str,
        instance: &str,
        request_id: &str,
        key: Option<&OpKey>,
        answer: RunnerFrame,
    ) {
        let run = key
            .cloned()
            .map_or_else(|| RunKey::Request(request_id.to_owned()), RunKey::Op);
        lock(&self.inner.running).remove(&(host.to_owned(), run));
        let (answer, waiting) = match key {
            Some(key) => lock(&self.inner.tables).answer(host, key, answer, Instant::now()),
            None => (answer, None),
        };
        let waiting = waiting.unwrap_or_else(|| instance.to_owned());
        if let Some(connection) = lock(&self.inner.hosts).get(&(host.to_owned(), waiting)) {
            let _ = connection.sender.send(answer);
        }
    }

    fn run_key(&self, host: &str, instance: &str, request_id: String) -> RunKey {
        lock(&self.inner.tables)
            .key_for(host, instance, &request_id)
            .map_or(RunKey::Request(request_id), RunKey::Op)
    }

    /// Answers a session verb from the line's sessions, made on first use.
    fn sessions(
        &self,
        host: &str,
        line: &str,
        verb: RunnerVerb,
        payload: Value,
        control: OperationContext,
    ) -> ServeFuture {
        let sessions = self.line(host, line, &payload);
        match sessions {
            Ok((sessions, environment)) => serve_sessions(
                &environment,
                &sessions,
                &self.inner.hello.instance,
                verb.as_str(),
                payload,
                control,
                self.inner.home_local.is_some(),
            ),
            Err(failure) => Box::pin(std::future::ready(Err(failure))),
        }
    }

    /// The line's sessions and their environment, made on first use from the
    /// workspace the request names.
    fn line(
        &self,
        host: &str,
        line: &str,
        payload: &Value,
    ) -> Result<(JoinedProcessSessions, ExecutionEnvironment), ServeFailure> {
        let mut lines = lock(&self.inner.lines);
        if let Some(found) = lines.get(&(host.to_owned(), line.to_owned())) {
            return Ok(found.clone());
        }
        let environment = self.environment_for(payload)?;
        let sessions = line_sessions(&environment, &self.inner.capacity)
            .map_err(|error| serve_failure(&error))?;
        let found = (sessions, environment);
        lines.insert((host.to_owned(), line.to_owned()), found.clone());
        Ok(found)
    }

    /// Opens the line's job event stream on this connection. Nothing is sent
    /// on it until the host grants credit.
    fn watch_jobs(&self, host: &str, connection: u64, line: &str, payload: &Value) -> ServeFuture {
        let (sessions, _) = match self.line(host, line, payload) {
            Ok(found) => found,
            Err(failure) => return Box::pin(std::future::ready(Err(failure))),
        };
        let events = serve_jobs_watch(&sessions, &self.inner.hello.instance);
        let stream = self.inner.next_stream.fetch_add(1, Ordering::Relaxed);
        self.carry(
            host,
            connection,
            stream,
            Box::pin(tokio::io::sink()),
            events,
            None,
        );
        Box::pin(std::future::ready(Ok(service_spawned(stream))))
    }

    fn serve_environment(
        &self,
        verb: RunnerVerb,
        payload: Value,
        control: OperationContext,
    ) -> ServeFuture {
        if verb == RunnerVerb::WorkspaceChanges && !self.inner.changes_git_enabled {
            use plakous_tools::internal::changes::{ChangesAnswer, ChangesRefusal};
            return Box::pin(std::future::ready(
                serde_json::to_value(ChangesAnswer::Refused(ChangesRefusal::VcsUnavailable))
                    .map_err(|_| ServeFailure::Unsupported),
            ));
        }
        match self.environment_for(&payload) {
            Ok(environment) => serve(&environment, verb.as_str(), payload, control),
            Err(failure) => Box::pin(std::future::ready(Err(failure))),
        }
    }

    /// The default environment or the one admitted for this request's workspace.
    fn environment_for(&self, payload: &Value) -> Result<ExecutionEnvironment, ServeFailure> {
        let default = &self.inner.environment;
        let Some((root, cwd)) = requested_workspace(payload) else {
            return Ok(default.clone());
        };
        // With a factory, every named workspace is admitted by it, the
        // default one included.
        let Some(workspaces) = &self.inner.workspaces else {
            // The default environment refuses another workspace.
            return Ok(default.clone());
        };
        let mut registered = lock(&self.inner.registered);
        if let Some(environment) = registered.get(&(root.clone(), cwd.clone())) {
            return Ok(environment.clone());
        }
        let environment = workspaces(&root, &cwd).map_err(|error| environment_failure(&error))?;
        registered.insert((root, cwd), environment.clone());
        Ok(environment)
    }

    /// Spawns a service and carries its stdio as a new stream of this
    /// connection. Nothing is sent on it until the host grants credit.
    fn spawn_service(
        &self,
        host: &str,
        connection: u64,
        payload: Value,
        control: OperationContext,
    ) -> ServeFuture {
        let environment = match self.environment_for(&payload) {
            Ok(environment) => environment,
            Err(failure) => return Box::pin(std::future::ready(Err(failure))),
        };
        let spawned = serve_service_spawn(
            &environment,
            payload,
            control,
            self.inner.secrets.as_deref(),
        );
        let service = self.clone();
        let host = host.to_owned();
        Box::pin(async move {
            let process = spawned.await?;
            let stream = service.inner.next_stream.fetch_add(1, Ordering::Relaxed);
            service.carry_service(&host, connection, stream, process);
            Ok(service_spawned(stream))
        })
    }

    fn carry_service(&self, host: &str, connection: u64, stream: u64, process: ServiceProcess) {
        let ServiceProcess {
            stdin,
            stdout,
            handle,
            ..
        } = process;
        self.carry(host, connection, stream, stdin, stdout, Some(handle));
    }

    /// Carries `input` and `output` as `stream` of `connection`, ending
    /// `handle` with it.
    fn carry(
        &self,
        host: &str,
        connection: u64,
        stream: u64,
        stdin: std::pin::Pin<Box<dyn AsyncWrite + Send>>,
        stdout: std::pin::Pin<Box<dyn AsyncRead + Send>>,
        handle: Option<ServiceHandle>,
    ) {
        let sender = self.connection_sender(host, connection);
        // The spawning connection ended while the service started: nothing
        // could carry its stdio, so it ends now.
        let Some(sender) = sender else {
            terminate_later(handle);
            return;
        };
        let out = Outbound::new(stream, move |frame: StreamFrame| {
            sender.send(RunnerFrame::Stream(frame)).is_ok()
        });
        let (chunks, received) = mpsc::unbounded_channel();
        let credit = Credit::new(0);
        let window = Window::new(INITIAL_CREDIT);
        {
            let mut services = lock(&self.inner.services);
            // Checked again under the services lock, which `end_services`
            // takes too, so a service is either ended with its connection or
            // never carried.
            let current = self.connection_sender(host, connection).is_some();
            if !current {
                drop(services);
                terminate_later(handle);
                return;
            }
            services.insert(
                (host.to_owned(), stream),
                Hosted {
                    ends: StreamEnds::new(
                        chunks,
                        Arc::clone(&credit),
                        Arc::clone(&window),
                        Arc::clone(&out),
                    ),
                    handle,
                    connection,
                },
            );
        }
        tokio::spawn(receive_stream(stdin, received, window, Arc::clone(&out)));
        tokio::spawn(send_stream(stdout, credit, out));
    }

    /// The sender of `host`'s connection `connection`, while it is current.
    fn connection_sender(
        &self,
        host: &str,
        connection: u64,
    ) -> Option<mpsc::UnboundedSender<RunnerFrame>> {
        lock(&self.inner.hosts)
            .iter()
            .find(|((owner, _), current)| owner == host && current.id == connection)
            .map(|(_, current)| current.sender.clone())
    }

    fn terminate_service(&self, host: &str, payload: Value) -> ServeFuture {
        let handle = terminated_service(&payload).and_then(|stream| {
            let mut services = lock(&self.inner.services);
            let mut hosted = services.remove(&(host.to_owned(), stream))?;
            hosted.ends.close();
            hosted.handle.take()
        });
        serve_service_terminate(handle, payload)
    }

    fn stream(&self, host: &str, frame: StreamFrame) {
        let mut services = lock(&self.inner.services);
        let key = (host.to_owned(), frame.stream);
        let Some(hosted) = services.get_mut(&key) else {
            return;
        };
        if !hosted.ends.receive(frame) {
            // A broken stream ends its service.
            if let Some(mut hosted) = services.remove(&key) {
                terminate_later(hosted.handle.take());
            }
        }
    }

    /// Ends the services `connection` spawned: their streams died with it.
    fn end_services(&self, host: &str, connection: u64) {
        let mut services = lock(&self.inner.services);
        let ended: Vec<(String, u64)> = services
            .iter()
            .filter(|((owner, _), hosted)| owner == host && hosted.connection == connection)
            .map(|(key, _)| key.clone())
            .collect();
        for key in ended {
            if let Some(mut hosted) = services.remove(&key) {
                hosted.ends.close();
                terminate_later(hosted.handle.take());
            }
        }
    }

    /// Losing the endpoint ends the daemon, not merely a host's link lease.
    pub(crate) async fn shutdown(&self) {
        self.inner.closing.store(true, Ordering::Release);
        if let Some(operator) = &self.inner.operator {
            operator.close_all();
        }
        for (_, connection) in std::mem::take(&mut *lock(&self.inner.hosts)) {
            connection.closer.cancel();
        }
        for running in lock(&self.inner.running).values() {
            running.cancellation.cancel();
        }
        self.stop_owned().await;
        // A start may have passed its fence before closing was set. Let its
        // cancellation/answer settle, then collect processes published late.
        if tokio::time::timeout(Duration::from_secs(5), async {
            while !lock(&self.inner.running).is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_err()
        {
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "runner shutdown still has in-flight calls after its cleanup bound"
            );
        }
        self.stop_owned().await;
        self.join_operator_shutdown().await;
    }

    async fn stop_owned(&self) {
        let mut cleanup = tokio::task::JoinSet::new();
        for (_, mut service) in std::mem::take(&mut *lock(&self.inner.services)) {
            service.ends.close();
            if let Some(handle) = service.handle.take() {
                cleanup.spawn(async move {
                    let _ = handle.terminate(Duration::from_secs(1)).await;
                });
            }
        }
        for (_, (sessions, _)) in std::mem::take(&mut *lock(&self.inner.lines)) {
            cleanup.spawn_blocking(move || {
                let _ = sessions.stop_foreground();
                let _ = sessions.stop_jobs(
                    plakous_tools::JobStopReason::SessionExit,
                    Duration::from_secs(1),
                );
            });
        }
        while cleanup.join_next().await.is_some() {}
    }
}

fn terminate_later(handle: Option<ServiceHandle>) {
    if let Some(handle) = handle {
        tokio::spawn(async move {
            let _ = handle.terminate(Duration::from_secs(1)).await;
        });
    }
}

/// How a running request is found again for cancellation and yields.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum RunKey {
    Op(OpKey),
    Request(String),
}

/// Clears what a line's jobs ran and printed, for an inspector that is not
/// the runner machine's own.
fn redact(line: &mut LineStatus) {
    let Some(jobs) = line.jobs.get_mut("jobs").and_then(Value::as_array_mut) else {
        return;
    };
    for job in jobs {
        for field in ["command", "output"] {
            if let Some(value) = job.get_mut(field) {
                *value = Value::String(String::new());
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

const fn refused(request_id: Option<String>, refusal: Refusal) -> RunnerFrame {
    RunnerFrame::Refused {
        request_id,
        refusal,
    }
}

fn answer_frame(request_id: String, served: Result<Value, ServeFailure>) -> RunnerFrame {
    let frame = match served {
        Ok(payload) => RunnerFrame::Result {
            request_id: request_id.clone(),
            payload,
        },
        Err(ServeFailure::Unsupported) => refused(
            Some(request_id.clone()),
            Refusal::Unsupported {
                what: "verb".into(),
            },
        ),
        Err(ServeFailure::Backend {
            error,
            proven_no_effect,
        }) => RunnerFrame::Error {
            request_id: request_id.clone(),
            error: WireError {
                error,
                proven_no_effect,
            },
        },
    };
    if fits(&frame) {
        return frame;
    }
    RunnerFrame::Error {
        request_id,
        error: WireError {
            error: plakous_tools::internal::runner::answer_too_large(),
            proven_no_effect: false,
        },
    }
}

/// The operation context the request names, as the runner's own clock,
/// cancellation and yield signal carry it.
fn context(
    request: &Request,
    cancellation: CancellationToken,
    yields: watch::Receiver<ToolPreempt>,
) -> Result<OperationContext, String> {
    let mut control = OperationContext::new(cancellation).with_preempt(yields);
    if let Some(owner) = &request.owner {
        control = control.with_owner(line_owner(owner)?);
    }
    if let RequestClass::EffectCall {
        call_id,
        attempt_id,
    } = &request.class
    {
        control = control.with_class(OperationClass::EffectCall {
            call_id: CallId::from_str(call_id).map_err(|error| error.to_string())?,
            attempt_id: AttemptId::from_str(attempt_id).map_err(|error| error.to_string())?,
        });
    }
    if let Some(remaining) = request.deadline_ms {
        let deadline = plakous_model::now_unix_millis().saturating_add(remaining);
        control = control.with_budget(
            Timestamp::new(
                deadline / 1000,
                u32::try_from((deadline % 1000) * 1_000_000).unwrap_or(0),
            )
            .map_err(|error| error.to_string())?,
        );
    }
    Ok(control)
}

pub(crate) fn line_owner(owner: &LineOwner) -> Result<plakous_tools::LineOwner, String> {
    Ok(plakous_tools::LineOwner::new(
        LineId::from_str(&owner.line).map_err(|error| error.to_string())?,
        owner.generation,
        BranchId::from_str(&owner.branch).map_err(|error| error.to_string())?,
        owner.fence,
    ))
}
