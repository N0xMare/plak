//! The runner end: answers host connections from a local execution
//! environment, with the owner and op tables, the per-line process sessions
//! and the services of this instance.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use plakous_model::{AttemptId, BranchId, CallId, CancellationToken, LineId, Timestamp};
use plakous_runner_protocol::frame::FrameError;
use plakous_runner_protocol::{
    Hello, HostFrame, LineOwner, Refusal, Request, RequestClass, RunnerFrame, StreamFrame,
    VersionTriple, WireError, capabilities,
};
use plakous_tools::internal::runner::{
    RunnerExtension, RunnerVerb, RunnerVerbFamily, ServeFailure, ServeFuture, decode_preempt,
    environment_failure, job_capacity, line_sessions, requested_workspace, runner_descriptor,
    serve, serve_failure, serve_probe, serve_service_spawn, serve_service_terminate,
    serve_sessions, service_spawned, terminated_service,
};
use plakous_tools::{
    ExecutionEnvironment, ExecutionEnvironmentError, JoinedProcessHostCapacity,
    JoinedProcessSessions, OperationClass, OperationContext, RunnerId, ServiceHandle,
    ServiceProcess, ToolPreempt,
};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, watch};

use crate::connection::{FrameStream, fits, write_frames};
use crate::state::{Admission, OpKey, Tables};
use crate::streams::{
    Credit, INITIAL_CREDIT, Outbound, StreamEnds, Window, receive_stream, send_stream,
};

/// One runner process: its identity, its environment and the state every
/// connection to it shares. A new service is a new instance.
#[derive(Clone)]
pub struct RunnerService {
    inner: Arc<Inner>,
}

struct Inner {
    environment: ExecutionEnvironment,
    workspaces: Option<Arc<WorkspaceFactory>>,
    /// Environments registered for workspaces other than the default one,
    /// by root and cwd as requests name them.
    registered: Mutex<HashMap<(PathBuf, PathBuf), ExecutionEnvironment>>,
    extension: Option<Arc<dyn RunnerExtension>>,
    hello: Hello,
    dead: Duration,
    capacity: JoinedProcessHostCapacity,
    tables: Mutex<Tables>,
    /// Each host's current connection: answers go there, so an answer that
    /// outlives its connection reaches the host's reconnection.
    hosts: Mutex<HashMap<String, Connection>>,
    next_connection: AtomicU64,
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
    host_instance: String,
    closer: CancellationToken,
    id: u64,
}

struct Running {
    cancellation: CancellationToken,
    preempt: watch::Sender<ToolPreempt>,
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
}

impl Default for RunnerLimits {
    fn default() -> Self {
        Self {
            job_bound: 128,
            dead: Duration::from_secs(15),
        }
    }
}

/// Makes the environment of a workspace a request names, by root and cwd,
/// the first time one does: a runner serves every workspace it can open.
pub type WorkspaceFactory =
    dyn Fn(&Path, &Path) -> Result<ExecutionEnvironment, ExecutionEnvironmentError> + Send + Sync;

/// What a runner serves beyond its default environment.
#[derive(Clone, Default)]
pub struct RunnerOptions {
    /// Its bounds.
    pub limits: RunnerLimits,
    /// Environments for other workspaces; without it, a request naming
    /// another workspace is refused.
    pub workspaces: Option<Arc<WorkspaceFactory>>,
    /// The verbs the embedding program answers.
    pub extension: Option<Arc<dyn RunnerExtension>>,
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
        let hello = Hello {
            version: identity.version,
            binary_id: identity.binary_id,
            runner_id: identity.runner_id.as_str().to_owned(),
            instance: uuid::Uuid::new_v4().to_string(),
            boot_id: identity.boot_id,
            descriptor,
            capabilities: vec![
                capabilities::WORKSPACE_VERBS.to_owned(),
                capabilities::STREAMS.to_owned(),
            ],
            path: None,
            home: None,
            job_bound: limits.job_bound,
        };
        Ok(Self {
            inner: Arc::new(Inner {
                environment,
                workspaces: options.workspaces,
                registered: Mutex::default(),
                extension: options.extension,
                hello,
                dead: limits.dead,
                capacity: job_capacity(usize::try_from(limits.job_bound).unwrap_or(usize::MAX)),
                tables: Mutex::default(),
                hosts: Mutex::default(),
                next_connection: AtomicU64::new(0),
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
    /// host's next connection; jobs keep running; its services end.
    pub async fn serve_connection<S>(&self, stream: S)
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (reader, writer) = tokio::io::split(stream);
        let (sender, frames) = mpsc::unbounded_channel();
        let closer = CancellationToken::new();
        tokio::spawn(write_frames(writer, frames, closer.clone()));
        let mut reader = FrameStream::new(reader);
        let _ = sender.send(RunnerFrame::Hello(self.inner.hello.clone()));
        let bind = tokio::time::timeout(self.inner.dead, reader.next::<HostFrame>()).await;
        let Ok(Some(Ok(HostFrame::Bind(bind)))) = bind else {
            let _ = sender.send(refused(None, Refusal::Unbound));
            return;
        };
        if bind.version != self.inner.hello.version {
            let _ = sender.send(refused(
                None,
                Refusal::VersionMismatch {
                    runner: self.inner.hello.version.clone(),
                },
            ));
            return;
        }
        let host = bind.host_id;
        let connection = self.inner.next_connection.fetch_add(1, Ordering::Relaxed);
        let previous = lock(&self.inner.hosts).insert(
            host.clone(),
            Connection {
                sender: sender.clone(),
                host_instance: bind.host_instance.clone(),
                closer: closer.clone(),
                id: connection,
            },
        );
        // A host instance binds again only once its older connection is
        // dead to it: that connection is drained, and its answers come here.
        if let Some(previous) = previous
            && previous.host_instance == bind.host_instance
        {
            previous.closer.cancel();
        }
        loop {
            let frame = tokio::select! {
                frame = tokio::time::timeout(self.inner.dead, reader.next::<HostFrame>()) => frame,
                () = closer.cancelled() => break,
            };
            let frame = match frame {
                Err(_) | Ok(None) => break,
                Ok(Some(Ok(frame))) => frame,
                Ok(Some(Err(error))) => {
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
            self.receive(&host, connection, frame, &sender);
        }
        // The connection is over: answers no longer go to it, its services
        // end, and its write half closes once what is queued is written.
        {
            let mut hosts = lock(&self.inner.hosts);
            if hosts
                .get(&host)
                .is_some_and(|current| current.id == connection)
            {
                hosts.remove(&host);
            }
        }
        self.end_services(&host, connection);
        closer.cancel();
    }

    fn receive(
        &self,
        host: &str,
        connection: u64,
        frame: HostFrame,
        sender: &mpsc::UnboundedSender<RunnerFrame>,
    ) {
        match frame {
            HostFrame::Request(request) => self.admit(host, connection, request, sender),
            HostFrame::Cancel { request_id } => {
                let run = self.run_key(host, request_id);
                if let Some(running) = lock(&self.inner.running).get(&(host.to_owned(), run)) {
                    running.cancellation.cancel();
                }
            }
            HostFrame::Preempt { request_id, value } => {
                let run = self.run_key(host, request_id);
                if let Some(running) = lock(&self.inner.running).get(&(host.to_owned(), run)) {
                    let _ = running.preempt.send(decode_preempt(&value));
                }
            }
            HostFrame::Ack { request_id } => {
                lock(&self.inner.tables).acknowledge(host, &request_id);
            }
            HostFrame::TransferOwner(transfer) => {
                let answer =
                    lock(&self.inner.tables).transfer(host, transfer.control_id, transfer.to);
                let _ = sender.send(answer);
            }
            HostFrame::Ping { nonce } => {
                let _ = sender.send(RunnerFrame::Pong { nonce });
            }
            HostFrame::Bind(_) => {
                let _ = sender.send(refused(
                    None,
                    Refusal::Malformed {
                        detail: "the connection is already bound".into(),
                    },
                ));
            }
            HostFrame::Stream(frame) => self.stream(host, frame),
        }
    }

    fn admit(
        &self,
        host: &str,
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
        let key = match lock(&self.inner.tables).admit(host, &request, Instant::now()) {
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
                cancellation,
                preempt,
            },
        );
        let service = self.clone();
        let host = host.to_owned();
        let payload = request.operation.payload;
        let served = match verb.family() {
            RunnerVerbFamily::Environment => match self.environment_for(&payload) {
                Ok(environment) => serve(&environment, verb.as_str(), payload, control),
                Err(failure) => Box::pin(std::future::ready(Err(failure))),
            },
            RunnerVerbFamily::Probe => serve_probe(payload),
            RunnerVerbFamily::Extension => match &self.inner.extension {
                Some(extension) => extension.serve(verb, payload),
                None => Box::pin(std::future::ready(Err(ServeFailure::Unsupported))),
            },
            RunnerVerbFamily::Sessions => {
                let line = request.owner.map(|owner| owner.line).unwrap_or_default();
                self.sessions(&host, &line, verb, payload, control)
            }
            RunnerVerbFamily::Service if verb == RunnerVerb::ServiceSpawn => {
                self.spawn_service(&host, connection, payload, control)
            }
            RunnerVerbFamily::Service => self.terminate_service(&host, payload),
        };
        tokio::spawn(async move {
            let answer = answer_frame(id.clone(), served.await);
            service.finish(&host, &id, key.as_ref(), answer);
        });
    }

    fn finish(&self, host: &str, request_id: &str, key: Option<&OpKey>, answer: RunnerFrame) {
        let run = key
            .cloned()
            .map_or_else(|| RunKey::Request(request_id.to_owned()), RunKey::Op);
        lock(&self.inner.running).remove(&(host.to_owned(), run));
        let answer = match key {
            Some(key) => lock(&self.inner.tables).answer(host, key, answer, Instant::now()),
            None => answer,
        };
        if let Some(connection) = lock(&self.inner.hosts).get(host) {
            let _ = connection.sender.send(answer);
        }
    }

    fn run_key(&self, host: &str, request_id: String) -> RunKey {
        lock(&self.inner.tables)
            .key_for(host, &request_id)
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
        let sessions = {
            let mut lines = lock(&self.inner.lines);
            match lines.get(&(host.to_owned(), line.to_owned())) {
                Some(found) => Ok(found.clone()),
                None => self.environment_for(&payload).and_then(|environment| {
                    line_sessions(&environment, &self.inner.capacity)
                        .map(|sessions| {
                            let found = (sessions, environment);
                            lines.insert((host.to_owned(), line.to_owned()), found.clone());
                            found
                        })
                        .map_err(|error| serve_failure(&error))
                }),
            }
        };
        match sessions {
            Ok((sessions, environment)) => serve_sessions(
                &environment,
                &sessions,
                &self.inner.hello.instance,
                verb.as_str(),
                payload,
                control,
            ),
            Err(failure) => Box::pin(std::future::ready(Err(failure))),
        }
    }

    /// The environment answering a request: the default one, or the one
    /// registered for the workspace the request names, made on first use.
    fn environment_for(&self, payload: &Value) -> Result<ExecutionEnvironment, ServeFailure> {
        let default = &self.inner.environment;
        let Some((root, cwd)) = requested_workspace(payload) else {
            return Ok(default.clone());
        };
        if root == default.root() && cwd == default.cwd() {
            return Ok(default.clone());
        }
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
        let spawned = serve_service_spawn(&environment, payload, control);
        let service = self.clone();
        let host = host.to_owned();
        Box::pin(async move {
            let process = spawned.await?;
            let stream = service.inner.next_stream.fetch_add(1, Ordering::Relaxed);
            service.carry(&host, connection, stream, process);
            Ok(service_spawned(stream))
        })
    }

    fn carry(&self, host: &str, connection: u64, stream: u64, process: ServiceProcess) {
        let ServiceProcess {
            stdin,
            stdout,
            handle,
            ..
        } = process;
        let sender = lock(&self.inner.hosts)
            .get(host)
            .filter(|current| current.id == connection)
            .map(|current| current.sender.clone());
        // The spawning connection ended while the service started: nothing
        // could carry its stdio, so it ends now.
        let Some(sender) = sender else {
            terminate_later(Some(handle));
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
            let current = lock(&self.inner.hosts)
                .get(host)
                .is_some_and(|current| current.id == connection);
            if !current {
                drop(services);
                terminate_later(Some(handle));
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
                    handle: Some(handle),
                    connection,
                },
            );
        }
        tokio::spawn(receive_stream(stdin, received, window, Arc::clone(&out)));
        tokio::spawn(send_stream(stdout, credit, out));
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
