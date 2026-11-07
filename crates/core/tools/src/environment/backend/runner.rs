//! The runner seam (unsupported, spec 01 §8.5): an execution backend whose
//! operations run on a runner reached through a [`RunnerLink`], a host handle
//! on one line's process sessions there ([`RunnerSessions`]), and the runner
//! side that answers both from a local [`ExecutionEnvironment`].
//!
//! Requests and results cross the link as JSON values in a schema this module
//! owns ([`wire`]); the link frames, keys and fences them. Nothing here
//! selects a runner for a session.

mod local;
mod probe;
mod service;
mod services;
mod sessions;
mod sessions_wire;
mod wire;

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

pub(in crate::environment) use local::LocalDaemon;
pub use local::{LocalDaemonBackend, local_daemon_environment};
use plakous_model::{AttemptId, CallId, CancellationToken, Timestamp};
pub use probe::{RunnerWorkspaceProbe, serve_probe};
use serde_json::Value;
pub(super) use service::RunnerServiceOwner;
pub use service::{
    RunnerSecretResolver, ServeServiceFuture, serve_service_spawn, serve_service_terminate,
    service_spawned, terminated_service,
};
pub use services::RunnerServices;

/// `request` naming `secrets` (variable, secret name) for the runner that
/// spawns it to resolve from its own store. A local backend refuses such a
/// request.
///
/// # Errors
///
/// Rejects an invalid secret name, a variable named twice, and too many
/// environment entries.
pub fn with_runner_secrets(
    request: crate::ServiceProcessRequest,
    secrets: Vec<(crate::EnvironmentVariableName, String)>,
) -> Result<crate::ServiceProcessRequest, ExecutionBackendError> {
    request.with_runner_secrets(secrets)
}
pub use sessions::{
    RunnerSessions, decode_jobs_snapshot, job_capacity, line_sessions, serve_jobs_watch,
    serve_sessions,
};
use tokio::io::{AsyncRead, AsyncWrite};

use super::{
    EnvironmentDelegation, EnvironmentVariableValue, ExecutionBackend, ExecutionBackendError,
    ExecutionBackendErrorKind, ExecutionBackendFuture, ExecutionBackendIdentity,
    ExecutionEnvironmentLimits, ExecutionWorkspace, LineOwner, LiteralProcessResult,
    OperationClass, OperationContext, PreparedLiteralProcess, PreparedServiceProcess,
    RepositoryQuery, RepositoryQueryResult, ServiceProcess, WorkspaceMutationTransaction,
    WorkspaceMutationTransactionResult, WorkspaceReadSlice, WorkspaceReadSliceRequest,
    backend_error, ready_error, sealed,
};
use crate::{
    ExecutionBackendDescriptor, ExecutionBackendKind, ExecutionEnvironment,
    ExecutionEnvironmentConfig, ExecutionEnvironmentError, ExecutionEnvironmentErrorKind,
    ExecutionEnvironmentScope, RunnerId, ToolPreempt,
};

/// What a runner answers for a workspace it does not serve: one outside the
/// roots it lists, or one holding its own home.
#[must_use]
pub fn workspace_refusal() -> ExecutionEnvironmentError {
    ExecutionEnvironmentError::new(
        ExecutionEnvironmentErrorKind::Workspace,
        "the runner does not serve this workspace: it lies outside the runner's roots or holds the runner home",
    )
}

/// The operations a runner answers through this seam.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerVerb {
    /// Effective workspace roots and default.
    WorkspaceOptions,
    /// Bounded checkout discovery on the runner.
    WorkspaceDiscover,
    /// Fresh canonical workspace admission on the runner.
    WorkspaceValidate,
    /// Keyed cleanup of one settled driver call's unpromoted process.
    ForegroundStop,
    /// [`ExecutionEnvironment::read_workspace`].
    ReadWorkspace,
    /// [`ExecutionEnvironment::query_repository`].
    QueryRepository,
    /// [`ExecutionEnvironment::mutate_workspace`].
    MutateWorkspace,
    /// [`ExecutionEnvironment::execute_literal_process`].
    ExecuteLiteralProcess,
    /// [`crate::JoinedProcessSessions::start`] on the owner's line.
    ProcessStart,
    /// [`crate::JoinedProcessSessions::write_stdin`].
    ProcessWrite,
    /// [`crate::JoinedProcessSessions::poll_from`], waiting for output.
    ProcessPoll,
    /// [`crate::JoinedProcessSessions::stop_with_grace`].
    ProcessStop,
    /// Promotes a foreground process to a job.
    ProcessPromote,
    /// [`crate::JoinedProcessSessions::jobs`], with the runner instance.
    JobsSnapshot,
    /// [`crate::JoinedProcessSessions::stop_jobs`].
    JobsStop,
    /// [`crate::JoinedProcessSessions::disown_leftovers`].
    JobsDisown,
    /// A stream of the line's job views, one line of JSON per change.
    JobsWatch,
    /// [`ExecutionEnvironment::spawn_service`], with its stdio as a stream.
    ServiceSpawn,
    /// [`crate::ServiceHandle::terminate`] of a spawned service.
    ServiceTerminate,
    /// One [`crate::WorkspaceProbe`] question.
    ProbeWorkspace,
    /// One instruction-source read, answered by the runner's registered
    /// extension.
    ReadInstructions,
    /// One version-control question about a workspace on the runner,
    /// answered by the runner's registered extension.
    VcsRead,
    /// One version-control effect on a workspace on the runner (a mint,
    /// seal, adopt, unmint, refresh or pin), answered by the runner's
    /// registered extension; sent as a tree operation.
    VcsEffect,
}

/// Which runner part answers a verb.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerVerbFamily {
    /// Authenticated workspace picker reads.
    Workspace,
    /// The execution environment ([`serve`]).
    Environment,
    /// The owner line's process sessions ([`serve_sessions`]).
    Sessions,
    /// Services and their streams.
    Service,
    /// Workspace probes ([`serve_probe`]).
    Probe,
    /// What the runner's [`RunnerExtension`] answers.
    Extension,
}

impl RunnerVerb {
    /// Every verb.
    pub const ALL: [Self; 23] = [
        Self::ForegroundStop,
        Self::WorkspaceOptions,
        Self::WorkspaceDiscover,
        Self::WorkspaceValidate,
        Self::ReadWorkspace,
        Self::QueryRepository,
        Self::MutateWorkspace,
        Self::ExecuteLiteralProcess,
        Self::ProcessStart,
        Self::ProcessWrite,
        Self::ProcessPoll,
        Self::ProcessStop,
        Self::ProcessPromote,
        Self::JobsSnapshot,
        Self::JobsStop,
        Self::JobsDisown,
        Self::JobsWatch,
        Self::ServiceSpawn,
        Self::ServiceTerminate,
        Self::ProbeWorkspace,
        Self::ReadInstructions,
        Self::VcsRead,
        Self::VcsEffect,
    ];

    /// The verb's wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WorkspaceOptions => "workspace.options",
            Self::WorkspaceDiscover => "workspace.discover",
            Self::WorkspaceValidate => "workspace.validate",
            Self::ForegroundStop => "process.foreground_stop",
            Self::ReadWorkspace => "workspace.read",
            Self::QueryRepository => "repository.query",
            Self::MutateWorkspace => "workspace.mutate",
            Self::ExecuteLiteralProcess => "process.execute",
            Self::ProcessStart => "process.start",
            Self::ProcessWrite => "process.write",
            Self::ProcessPoll => "process.poll",
            Self::ProcessStop => "process.stop",
            Self::ProcessPromote => "process.promote",
            Self::JobsSnapshot => "jobs.snapshot",
            Self::JobsStop => "jobs.stop",
            Self::JobsDisown => "jobs.disown",
            Self::JobsWatch => "jobs.watch",
            Self::ServiceSpawn => "service.spawn",
            Self::ServiceTerminate => "service.terminate",
            Self::ProbeWorkspace => "probe.workspace",
            Self::ReadInstructions => "instructions.read",
            Self::VcsRead => "vcs.read",
            Self::VcsEffect => "vcs.effect",
        }
    }

    /// The verb a wire name names, if any.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|verb| verb.as_str() == name)
    }

    /// Which runner part answers it.
    #[must_use]
    pub const fn family(self) -> RunnerVerbFamily {
        match self {
            Self::WorkspaceOptions | Self::WorkspaceDiscover | Self::WorkspaceValidate => {
                RunnerVerbFamily::Workspace
            }
            Self::ReadWorkspace
            | Self::QueryRepository
            | Self::MutateWorkspace
            | Self::ExecuteLiteralProcess => RunnerVerbFamily::Environment,
            Self::ServiceSpawn | Self::ServiceTerminate => RunnerVerbFamily::Service,
            Self::ProbeWorkspace => RunnerVerbFamily::Probe,
            Self::ReadInstructions | Self::VcsRead | Self::VcsEffect => RunnerVerbFamily::Extension,
            Self::ProcessStart
            | Self::ForegroundStop
            | Self::ProcessWrite
            | Self::ProcessPoll
            | Self::ProcessStop
            | Self::ProcessPromote
            | Self::JobsSnapshot
            | Self::JobsStop
            | Self::JobsDisown
            | Self::JobsWatch => RunnerVerbFamily::Sessions,
        }
    }

    /// Whether sending it again cannot repeat an effect.
    #[must_use]
    pub const fn is_read(self) -> bool {
        matches!(
            self,
            Self::WorkspaceOptions
                | Self::WorkspaceDiscover
                | Self::WorkspaceValidate
                | Self::ReadWorkspace
                | Self::QueryRepository
                | Self::ProcessPoll
                | Self::JobsSnapshot
                | Self::ProbeWorkspace
                | Self::ReadInstructions
                | Self::VcsRead
        )
    }
}

/// How a link keys one call (spec 01 §8.4).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunnerClass {
    /// One tool call's effect: at most once per key.
    Effect {
        /// The tool call.
        call_id: CallId,
        /// The call's attempt.
        attempt_id: AttemptId,
    },
    /// Work no tool call keys: never recorded.
    Unkeyed,
    /// An observation of one job from a cursor: idempotent, never recorded.
    Observation {
        /// The job's number.
        job: u32,
        /// The output position.
        cursor: u64,
    },
    /// A control of the owner line's processes: the link mints its control
    /// id, and it runs at most once per id.
    Control,
    /// A durable host intent's control, stable across host restarts.
    RecordedControl(String),
    /// One step of a tree operation: at most once per tree and operation
    /// id, fenced by the tree's custody rather than a line owner.
    Tree {
        /// The tree.
        tree_id: String,
        /// The operation the tree recorded for the step.
        operation_id: String,
    },
}

impl From<OperationClass> for RunnerClass {
    fn from(class: OperationClass) -> Self {
        match class {
            OperationClass::EffectCall {
                call_id,
                attempt_id,
            } => Self::Effect {
                call_id,
                attempt_id,
            },
            OperationClass::Unkeyed => Self::Unkeyed,
        }
    }
}

/// One operation a [`RunnerBackend`] or [`RunnerSessions`] hands its link.
#[derive(Debug)]
pub struct RunnerCall {
    verb: RunnerVerb,
    payload: Value,
    owner: Option<LineOwner>,
    class: RunnerClass,
    budget: Option<Timestamp>,
    cancellation: CancellationToken,
    preempt: tokio::sync::watch::Receiver<ToolPreempt>,
}

impl RunnerCall {
    /// A line control keyed by an already recorded host intent.
    #[must_use]
    pub fn recorded_control(
        verb: RunnerVerb,
        payload: Value,
        owner: LineOwner,
        intent: String,
    ) -> Self {
        let mut call = Self::unkeyed(verb, payload);
        call.owner = Some(owner);
        call.class = RunnerClass::RecordedControl(intent);
        call
    }
    fn new(verb: RunnerVerb, payload: Value, control: &OperationContext) -> Self {
        Self {
            verb,
            payload,
            owner: control.owner().copied(),
            class: control.class().into(),
            budget: control.budget(),
            cancellation: control.cancellation().clone(),
            preempt: control.preempt().clone(),
        }
    }

    /// An unkeyed call no line owns: a host's own question to its runner.
    #[must_use]
    pub fn unkeyed(verb: RunnerVerb, payload: Value) -> Self {
        Self::new(
            verb,
            payload,
            &OperationContext::new(CancellationToken::new()),
        )
    }

    /// A call no line owns that runs one step of `tree_id`'s operation
    /// `operation_id`: at most once per key, and replayed by it.
    #[must_use]
    pub fn tree(
        verb: RunnerVerb,
        payload: Value,
        tree_id: impl Into<String>,
        operation_id: impl Into<String>,
    ) -> Self {
        Self::unkeyed(verb, payload).with_class(RunnerClass::Tree {
            tree_id: tree_id.into(),
            operation_id: operation_id.into(),
        })
    }

    fn with_class(mut self, class: RunnerClass) -> Self {
        self.class = class;
        self
    }

    /// The operation.
    #[must_use]
    pub const fn verb(&self) -> RunnerVerb {
        self.verb
    }

    /// The operation's request, in this seam's schema.
    #[must_use]
    pub const fn payload(&self) -> &Value {
        &self.payload
    }

    /// The line head the operation runs for, when one started it.
    #[must_use]
    pub const fn owner(&self) -> Option<&LineOwner> {
        self.owner.as_ref()
    }

    /// The request class and key.
    #[must_use]
    pub const fn class(&self) -> &RunnerClass {
        &self.class
    }

    /// The caller's absolute deadline, when it set one.
    #[must_use]
    pub const fn budget(&self) -> Option<Timestamp> {
        self.budget
    }

    /// The caller's cancellation: the link asks the runner to cancel once it
    /// fires.
    #[must_use]
    pub const fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// The caller's yield signal: the link forwards each change to the
    /// runner while the call runs.
    #[must_use]
    pub const fn preempt(&self) -> &tokio::sync::watch::Receiver<ToolPreempt> {
        &self.preempt
    }
}

/// A yield in this seam's schema.
#[must_use]
pub fn encode_preempt(preempt: ToolPreempt) -> Value {
    Value::String(
        match preempt {
            ToolPreempt::None => "none",
            ToolPreempt::Steer => "steer",
            ToolPreempt::Notice(false) => "notice",
            ToolPreempt::Notice(true) => "notice_mail",
            ToolPreempt::Background => "background",
        }
        .to_owned(),
    )
}

/// The yield a value names; anything else is no yield.
#[must_use]
pub fn decode_preempt(value: &Value) -> ToolPreempt {
    match value.as_str() {
        Some("steer") => ToolPreempt::Steer,
        Some("notice") => ToolPreempt::Notice(false),
        Some("notice_mail") => ToolPreempt::Notice(true),
        Some("background") => ToolPreempt::Background,
        _ => ToolPreempt::None,
    }
}

/// Why a link produced no result.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RunnerFailure {
    /// The operation ran, or the runner's backend refused it, with this error
    /// in this seam's schema.
    Backend(Value),
    /// The runner accepts a newer owner of the line; nothing ran.
    StaleOwner,
    /// Another process of this host holds the line, so the runner answered
    /// nothing to this one; nothing ran, and the call is the holder's to
    /// answer.
    NotHolder,
    /// The runner does not implement the operation; nothing ran.
    Unsupported,
    /// The link refused the call without sending it.
    Unavailable(String),
    /// The call may have run: the link was lost after it was sent, or the
    /// runner restarted under it.
    OutcomeUnknown(String),
    /// The runner ran this key to its end, and its answer was taken before:
    /// what it returned is unknown, but nothing of it still runs.
    Spent,
    /// The runner refused the call as malformed or out of protocol; nothing
    /// ran.
    Refused(String),
}

/// An owned future answering one [`RunnerCall`].
pub type RunnerCallFuture = Pin<Box<dyn Future<Output = Result<Value, RunnerFailure>> + Send>>;

/// The two directions of one runner stream: bytes written to `input` reach
/// the runner, and bytes the runner sends arrive on `output`.
pub struct RunnerStreams {
    /// Toward the runner.
    pub input: Pin<Box<dyn AsyncWrite + Send>>,
    /// From the runner.
    pub output: Pin<Box<dyn AsyncRead + Send>>,
}

impl std::fmt::Debug for RunnerStreams {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RunnerStreams")
            .finish_non_exhaustive()
    }
}

/// An owned future answering one [`RunnerCall`] whose result opens a stream.
pub type RunnerOpenFuture =
    Pin<Box<dyn Future<Output = Result<(Value, RunnerStreams), RunnerFailure>> + Send>>;

/// How a [`RunnerBackend`] reaches its runner. The implementation frames,
/// keys and fences each call; it never interprets the payload.
pub trait RunnerLink: Send + Sync + 'static {
    /// Sends one call and answers its result.
    fn call(&self, call: RunnerCall) -> RunnerCallFuture;

    /// Sends one call whose result opens a stream. A link without streams
    /// answers [`RunnerFailure::Unsupported`].
    fn open(&self, call: RunnerCall) -> RunnerOpenFuture {
        drop(call);
        Box::pin(std::future::ready(Err(RunnerFailure::Unsupported)))
    }
}

/// Answers the verbs of [`RunnerVerbFamily::Extension`] on a runner: what
/// the embedding program registers beyond the execution environment, such as
/// instruction-source reads.
pub trait RunnerExtension: Send + Sync + 'static {
    /// Answers one call of `verb`.
    fn serve(&self, verb: RunnerVerb, payload: Value) -> ServeFuture;
}

/// Sends `call` and waits for its answer on a thread of its own, for a
/// synchronous port whose callers run off the async workers; `runtime` must
/// be a multi-thread runtime that drives the link.
///
/// # Errors
/// The link's failure, or an outcome-unknown failure when the waiting
/// thread failed.
pub fn call_blocking(
    link: &dyn RunnerLink,
    runtime: &tokio::runtime::Handle,
    call: RunnerCall,
) -> Result<Value, RunnerFailure> {
    let answer = link.call(call);
    std::thread::scope(|scope| scope.spawn(|| runtime.block_on(answer)).join()).unwrap_or_else(
        |_| {
            Err(RunnerFailure::OutcomeUnknown(
                "the waiting thread failed".into(),
            ))
        },
    )
}

/// An execution backend whose operations run on a runner. Its identity is
/// always a runner descriptor naming a runner other than `local`; it is never
/// selected for a session by this crate.
#[derive(Clone)]
pub struct RunnerBackend {
    identity: ExecutionBackendIdentity,
    runner_id: RunnerId,
    link: Arc<dyn RunnerLink>,
    runtime: Option<tokio::runtime::Handle>,
}

impl RunnerBackend {
    /// A backend presenting the runner's `descriptor` (as its hello carried
    /// it) over `link`.
    ///
    /// # Errors
    ///
    /// Refuses a descriptor that does not decode, is not a runner descriptor,
    /// or names the reserved `local` runner.
    pub fn new(
        descriptor: Value,
        link: Arc<dyn RunnerLink>,
    ) -> Result<Self, ExecutionEnvironmentError> {
        let invalid = || {
            ExecutionEnvironmentError::new(
                ExecutionEnvironmentErrorKind::InvalidIdentity,
                "runner descriptor is invalid",
            )
        };
        let descriptor: ExecutionBackendDescriptor =
            serde_json::from_value(descriptor).map_err(|_| invalid())?;
        let runner_id = match descriptor.runner_id() {
            Some(runner)
                if descriptor.kind() == ExecutionBackendKind::Runner && !runner.is_local() =>
            {
                runner.clone()
            }
            _ => return Err(invalid()),
        };
        Ok(Self {
            identity: ExecutionBackendIdentity::runner(descriptor)?,
            runner_id,
            link,
            runtime: tokio::runtime::Handle::try_current().ok(),
        })
    }

    /// Pins the executor used for synchronous construction reads on this link.
    #[must_use]
    pub fn running_on(mut self, runtime: tokio::runtime::Handle) -> Self {
        self.runtime = Some(runtime);
        self
    }

    /// The runner this backend's operations run on.
    #[must_use]
    pub const fn runner_id(&self) -> &RunnerId {
        &self.runner_id
    }

    /// The link this backend sends through.
    #[must_use]
    pub fn link(&self) -> Arc<dyn RunnerLink> {
        Arc::clone(&self.link)
    }

    fn call<T: Send + 'static>(
        &self,
        verb: RunnerVerb,
        workspace: &ExecutionWorkspace,
        request: Result<Value, ExecutionBackendError>,
        control: &OperationContext,
        decode: fn(Value) -> Result<T, ExecutionBackendError>,
    ) -> ExecutionBackendFuture<T> {
        let payload = match request.and_then(|request| wire::envelope(workspace, request)) {
            Ok(payload) => payload,
            Err(error) => return ready_error(error),
        };
        let answer = self.link.call(RunnerCall::new(verb, payload, control));
        Box::pin(async move {
            match answer.await {
                Ok(value) => decode(value),
                Err(failure) => Err(failure_error(failure)),
            }
        })
    }
}

fn failure_error(failure: RunnerFailure) -> ExecutionBackendError {
    match failure {
        RunnerFailure::Backend(error) => wire::decode_error(error),
        RunnerFailure::StaleOwner => ExecutionBackendError::new(
            ExecutionBackendErrorKind::IdentityMismatch,
            "a newer owner of this agent line holds it on the runner",
        ),
        RunnerFailure::Unsupported => backend_error(
            ExecutionBackendErrorKind::Unsupported,
            "the runner does not implement this operation",
        ),
        RunnerFailure::Unavailable(detail) => ExecutionBackendError::new(
            ExecutionBackendErrorKind::Io,
            format!("the runner is unavailable: {detail}"),
        )
        .with_proven_no_effect(true),
        RunnerFailure::OutcomeUnknown(detail) => ExecutionBackendError::new(
            ExecutionBackendErrorKind::RecoveryRequired,
            format!("the runner operation's outcome is unknown: {detail}"),
        ),
        RunnerFailure::Spent => ExecutionBackendError::new(
            ExecutionBackendErrorKind::RecoveryRequired,
            "the runner ran this call and its answer was already taken, so its outcome is unknown",
        ),
        RunnerFailure::Refused(detail) => ExecutionBackendError::new(
            ExecutionBackendErrorKind::InvalidRequest,
            format!("the runner refused the operation: {detail}"),
        ),
        RunnerFailure::NotHolder => ExecutionBackendError::new(
            ExecutionBackendErrorKind::InvalidRequest,
            "the runner refused the operation: another process of this host holds the agent line",
        ),
    }
}

impl sealed::Sealed for RunnerBackend {}

impl ExecutionBackend for RunnerBackend {
    fn identity(&self) -> &ExecutionBackendIdentity {
        &self.identity
    }

    /// The runner resolves its own delegations from its own environment.
    fn resolve_environment(
        &self,
        _binding: &EnvironmentDelegation,
    ) -> Option<EnvironmentVariableValue> {
        None
    }

    fn read_workspace(
        &self,
        workspace: ExecutionWorkspace,
        request: WorkspaceReadSliceRequest,
        control: OperationContext,
    ) -> ExecutionBackendFuture<WorkspaceReadSlice> {
        self.call(
            RunnerVerb::ReadWorkspace,
            &workspace,
            wire::encode_read_request(&request),
            &control,
            wire::decode_read_slice,
        )
    }

    fn query_repository(
        &self,
        workspace: ExecutionWorkspace,
        query: RepositoryQuery,
        control: OperationContext,
    ) -> ExecutionBackendFuture<RepositoryQueryResult> {
        self.call(
            RunnerVerb::QueryRepository,
            &workspace,
            wire::encode_query(&query),
            &control,
            wire::decode_query_result,
        )
    }

    fn mutate_workspace(
        &self,
        workspace: ExecutionWorkspace,
        transaction: WorkspaceMutationTransaction,
        control: OperationContext,
    ) -> ExecutionBackendFuture<WorkspaceMutationTransactionResult> {
        self.call(
            RunnerVerb::MutateWorkspace,
            &workspace,
            wire::encode_transaction(&transaction),
            &control,
            wire::decode_transaction_result,
        )
    }

    /// The runner prepares the request with its own delegations and applies
    /// its own environment's limits.
    fn execute_literal_process(
        &self,
        workspace: ExecutionWorkspace,
        request: PreparedLiteralProcess,
        _limits: ExecutionEnvironmentLimits,
        control: OperationContext,
    ) -> ExecutionBackendFuture<LiteralProcessResult> {
        self.call(
            RunnerVerb::ExecuteLiteralProcess,
            &workspace,
            wire::encode_process_request(request.request()),
            &control,
            wire::decode_process_result,
        )
    }

    /// The service runs on the runner with the runner's own delegations, and
    /// its stdio is carried as one stream. Secret environment values do not
    /// cross the link.
    fn spawn_service(
        &self,
        workspace: ExecutionWorkspace,
        request: PreparedServiceProcess,
        control: OperationContext,
    ) -> ExecutionBackendFuture<ServiceProcess> {
        service::spawn(&self.link, &workspace, &request.request, &control)
    }
}

/// An environment whose operations run through `backend`, over the workspace
/// `root` and `cwd` as they are named on the runner.
///
/// # Errors
///
/// Returns a workspace error, as [`ExecutionEnvironment::host`] does.
pub fn runner_environment(
    scope: ExecutionEnvironmentScope,
    root: impl AsRef<Path>,
    cwd: impl AsRef<Path>,
    config: ExecutionEnvironmentConfig,
    backend: Arc<RunnerBackend>,
) -> Result<ExecutionEnvironment, ExecutionEnvironmentError> {
    let link = backend.link();
    let runtime = backend
        .runtime
        .clone()
        .or_else(|| tokio::runtime::Handle::try_current().ok())
        .ok_or_else(workspace_refusal)?;
    let probe = RunnerWorkspaceProbe::new(Arc::clone(&link), runtime.clone());
    let workspace = crate::WorkspacePolicy::on_runner(root.as_ref(), cwd.as_ref(), &probe)
        .map_err(|_| workspace_refusal())?;
    let root_object = crate::WorkspaceProbe::filesystem_identity(&probe, workspace.root())
        .map_err(|_| workspace_refusal())?;
    let backend: Arc<dyn ExecutionBackend> = backend;
    let mut environment = ExecutionEnvironment::from_workspace_backend_and_identity(
        scope,
        workspace,
        config,
        backend,
        Some(root_object),
    )?;
    environment.runner_link = Some(link);
    environment.runner_runtime = Some(runtime);
    Ok(environment)
}

impl ExecutionEnvironment {
    /// Pins the executor of this node's runner link. This live service fact
    /// is not part of its frozen execution descriptor.
    #[must_use]
    pub fn with_runner_runtime(mut self, runtime: tokio::runtime::Handle) -> Self {
        self.runner_runtime = Some(runtime);
        self
    }
}

/// The descriptor a runner presents for `environment`'s backend under the
/// runner identity `runner`, as its hello carries it.
///
/// # Errors
///
/// Refuses the reserved `local` runner id.
pub fn runner_descriptor(
    environment: &ExecutionEnvironment,
    runner: RunnerId,
) -> Result<Value, ExecutionEnvironmentError> {
    if runner.is_local() {
        return Err(ExecutionEnvironmentError::new(
            ExecutionEnvironmentErrorKind::InvalidIdentity,
            "a runner presents a runner id other than local",
        ));
    }
    let mut descriptor = environment.descriptor().backend().clone();
    descriptor.runner_id = Some(runner);
    serde_json::to_value(&descriptor).map_err(|_| {
        ExecutionEnvironmentError::new(
            ExecutionEnvironmentErrorKind::InvalidIdentity,
            "runner descriptor could not be encoded",
        )
    })
}

/// Why the runner side answered no result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServeFailure {
    /// The verb is not one this part answers; nothing ran.
    Unsupported,
    /// The operation ran, or was refused, with this error in this seam's
    /// schema.
    Backend {
        /// The error.
        error: Value,
        /// Whether the operation provably had no effect.
        proven_no_effect: bool,
    },
}

/// An owned future answering one served operation.
pub type ServeFuture = Pin<Box<dyn Future<Output = Result<Value, ServeFailure>> + Send>>;

/// Answers one environment operation from `environment`, as the runner side
/// of a link. The request must name `environment`'s own workspace; a verb of
/// another family is [`ServeFailure::Unsupported`].
pub fn serve(
    environment: &ExecutionEnvironment,
    verb: &str,
    payload: Value,
    control: OperationContext,
) -> ServeFuture {
    let Some(verb) =
        RunnerVerb::parse(verb).filter(|verb| verb.family() == RunnerVerbFamily::Environment)
    else {
        return Box::pin(std::future::ready(Err(ServeFailure::Unsupported)));
    };
    let request = match wire::open_envelope(environment, payload) {
        Ok(request) => request,
        Err(error) => return Box::pin(std::future::ready(Err(backend_failure(&error)))),
    };
    let environment = environment.clone();
    Box::pin(async move {
        let answer = match verb {
            RunnerVerb::ReadWorkspace => match wire::decode_read_request(request) {
                Ok(request) => environment
                    .read_workspace(request, control)
                    .await
                    .and_then(|slice| wire::encode_read_slice(&slice)),
                Err(error) => Err(error),
            },
            RunnerVerb::QueryRepository => match wire::decode_query(request) {
                Ok(query) => environment
                    .query_repository(query, control)
                    .await
                    .and_then(|result| wire::encode_query_result(&result)),
                Err(error) => Err(error),
            },
            RunnerVerb::MutateWorkspace => match wire::decode_transaction(request) {
                Ok(transaction) => environment
                    .mutate_workspace(transaction, control)
                    .await
                    .and_then(|result| wire::encode_transaction_result(&result)),
                Err(error) => Err(error),
            },
            _ => match wire::decode_process_request(request) {
                Ok(request) => environment
                    .execute_literal_process(request, control)
                    .await
                    .and_then(|result| wire::encode_process_result(&result)),
                Err(error) => Err(error),
            },
        };
        answer.map_err(|error| backend_failure(&error))
    })
}

/// The workspace root and cwd a request names, as the runner knows them;
/// `None` for a request that names none. A runner uses it to find, or
/// register, the environment that answers the request.
#[must_use]
pub fn requested_workspace(payload: &Value) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    wire::envelope_workspace(payload)
}

/// Waits for `effect` on a thread of its own, so a synchronous caller on
/// any thread, async worker or not, gets the runner's answer.
pub(crate) fn wait_on<T: Send>(
    runtime: &tokio::runtime::Handle,
    effect: impl Future<Output = T> + Send,
) -> Result<T, ExecutionBackendError> {
    std::thread::scope(|scope| scope.spawn(|| runtime.block_on(effect)).join())
        .map_err(|_| waiter_failed())
}

/// `environment`'s process-session service on its runner, whose link runs
/// on `runtime`, adopted as the environment's service, its job events
/// reporting at once: a session on another runner without a durable restore.
///
/// # Errors
///
/// The service could not be allocated.
pub fn adopt_runner_process_sessions(
    environment: &ExecutionEnvironment,
    runtime: &tokio::runtime::Handle,
) -> Result<crate::JoinedProcessSessions, ExecutionBackendError> {
    let sessions = environment.runner_process_sessions(runtime)?;
    sessions.settle_runner_jobs();
    environment.adopt_joined_process_sessions(&sessions);
    Ok(sessions)
}

/// The error of a runner effect whose waiting thread failed: it may have run.
pub(crate) fn waiter_failed() -> ExecutionBackendError {
    ExecutionBackendError::new(
        ExecutionBackendErrorKind::RecoveryRequired,
        "the runner effect's waiter failed; its outcome is unknown",
    )
}

/// The error a runner answers in place of a result too large for one frame.
/// The operation ran, so its outcome is not proven to have had no effect.
#[must_use]
pub fn answer_too_large() -> Value {
    wire::encode_error(
        &backend_error(
            ExecutionBackendErrorKind::ResourceLimit,
            "the runner's answer exceeds its frame limit",
        )
        .with_proven_no_effect(false),
    )
}

/// The answer to a request for a workspace the runner could not open; it
/// had no effect.
#[must_use]
pub fn environment_failure(error: &ExecutionEnvironmentError) -> ServeFailure {
    backend_failure(&ExecutionBackendError::new(
        ExecutionBackendErrorKind::WorkspacePolicy,
        format!("the runner could not open the workspace: {error}"),
    ))
}

/// A runner-side error as the answer it becomes.
#[must_use]
pub fn serve_failure(error: &ExecutionBackendError) -> ServeFailure {
    backend_failure(error)
}

/// A foreground effect fenced out before its process dispatch.
#[must_use]
pub fn lost_process_owner() -> ServeFailure {
    backend_failure(&ExecutionBackendError::new(
        ExecutionBackendErrorKind::IdentityMismatch,
        "the process owner lost authority before dispatch",
    ))
}

fn backend_failure(error: &ExecutionBackendError) -> ServeFailure {
    ServeFailure::Backend {
        error: wire::encode_error(error),
        proven_no_effect: error.proven_no_effect(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
