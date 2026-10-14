//! Sealed execution backends and bounded provider-neutral operations.

mod process_types;
pub use process_types::{
    JoinedProcessCommand, JoinedProcessCursorPollResult, JoinedProcessInputDisposition,
    JoinedProcessOutputCursor, JoinedProcessOutputRecord, JoinedProcessOutputStream,
    JoinedProcessPollResult, JoinedProcessSessionId, JoinedProcessShellProfile,
    JoinedProcessShellProfileIdentity, JoinedProcessStartRequest, JoinedProcessStatus,
    JoinedProcessTerminalMode, LiteralProcessIntent, LiteralProcessRequest, LiteralProcessResult,
};
mod workspace_types;
pub use workspace_types::{
    RepositoryMatchCase, RepositoryQuery, RepositoryQueryLimits, RepositoryQueryMatch,
    RepositoryQueryResult, RepositoryQueryStopReason, WorkspaceContentIdentity,
    WorkspaceExpectedFileState, WorkspaceMutation, WorkspaceMutationDisposition,
    WorkspaceMutationResultItem, WorkspaceMutationTransaction, WorkspaceMutationTransactionResult,
    WorkspaceReadCursor, WorkspaceReadSlice, WorkspaceReadSliceRequest, WorkspaceReplacement,
};
mod deterministic;
pub use deterministic::{
    DeterministicBackendStatus, DeterministicExecutionBackend, DeterministicProcessPolicy,
    DeterministicProcessStep, RecordedLiteralProcess,
};
mod workspace;
use workspace::{
    WorkspaceSnapshotCache, mutate_workspace_sync, query_repository_sync, read_workspace_sync,
    run_filesystem_blocking,
};
mod job_output;
pub use job_output::{
    JOB_OUTPUT_RETENTION, JOB_OUTPUT_ROOT_BUDGET_BYTES, JOB_OUTPUT_STALE_AGE,
    JOB_OUTPUT_TRUNCATED_LINE, JobOutputFile, JobOutputRoot, JobOutputSlice,
    MAX_JOB_OUTPUT_FILE_BYTES, job_output_file_lost, job_output_incomplete_line,
};
mod process;
pub(super) use process::{HostProcessContext, JoinedProcessServiceCache};
pub use process::{JoinedProcessHostCapacity, JoinedProcessSessions};
use process::{
    execute_host_literal_process, validate_absolute_program, validate_process_value,
    validate_process_values,
};
use workspace::resolve_existing_no_symlink;

use plakous_model::{CancellationToken, SafeDiagnostic};
use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use super::{
    EnvironmentDelegation, EnvironmentVariableName, ExecutionBackendIdentity, ExecutionEnvironment,
    ExecutionEnvironmentConfig, ExecutionEnvironmentError, ExecutionEnvironmentErrorKind,
    ExecutionEnvironmentLimits, MAX_ENVIRONMENT_OUTPUT_BYTES,
};
use crate::ToolExecutorBindingIdentity;

/// Maximum UTF-8 bytes in one workspace path or literal process value.
pub const MAX_EXECUTION_VALUE_BYTES: usize = 65_536;
/// Maximum literal process argument count.
pub const MAX_LITERAL_PROCESS_ARGUMENTS: usize = 256;
/// Maximum literal process stdin bytes.
pub const MAX_LITERAL_PROCESS_STDIN_BYTES: usize = 16 * 1024 * 1024;
/// Maximum entries returned by one workspace walk.
pub const MAX_WORKSPACE_WALK_ENTRIES: usize = 10_000;
/// Maximum matches returned by one repository query.
pub const MAX_REPOSITORY_QUERY_MATCHES: usize = 4_096;
/// Maximum complete file enrolled by canonical workspace reads and searches.
pub const MAX_WORKSPACE_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// Maximum bytes returned by one canonical workspace-read page.
pub const MAX_WORKSPACE_READ_SLICE_BYTES: u64 = 4 * 1024 * 1024;
/// Exact workspace-content digest revision.
pub const WORKSPACE_CONTENT_IDENTITY_REVISION: u32 = 1;
/// Maximum files named by one workspace mutation.
pub const MAX_WORKSPACE_MUTATION_ITEMS: usize = 256;
/// Maximum process sessions owned by one process-local host service.
pub const MAX_JOINED_PROCESS_SESSIONS: usize = 64;
/// Maximum concurrent callers admitted to one session's serialized operation lane.
pub const MAX_JOINED_PROCESS_SESSION_QUEUE: usize = 64;
/// Maximum bytes accepted by one session write.
pub const MAX_JOINED_PROCESS_SESSION_WRITE_BYTES: usize = 64 * 1024;
/// Maximum output records returned by one poll.
pub const MAX_JOINED_PROCESS_SESSION_POLL_RECORDS: usize = 256;
/// Initial Unix PTY row count.
pub const JOINED_PROCESS_PTY_INITIAL_ROWS: u16 = 24;
/// Initial Unix PTY column count.
pub const JOINED_PROCESS_PTY_INITIAL_COLUMNS: u16 = 80;
/// Exact joined-process shell enrollment semantic revision.
pub const JOINED_PROCESS_SHELL_PROFILE_SEMANTIC_REVISION: u32 = 1;

static JOINED_PROCESS_SERVICE_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Owned future returned by an object-safe execution backend.
pub type ExecutionBackendFuture<T> =
    Pin<Box<dyn Future<Output = Result<T, ExecutionBackendError>> + Send + 'static>>;

/// Stable category for one bounded execution-backend failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ExecutionBackendErrorKind {
    /// The live backend authority no longer matches its environment.
    IdentityMismatch,
    /// The request is malformed or exceeds a structural bound.
    InvalidRequest,
    /// Workspace containment or file type policy rejected a path.
    WorkspacePolicy,
    /// The path names nothing in the workspace.
    NotFound,
    /// The path names a directory where the operation needs a file.
    IsADirectory,
    /// The path names something that is neither a file nor a directory: a
    /// socket, a device, a FIFO.
    NotARegularFile,
    /// The path resolves outside the workspace root.
    ///
    /// Distinct from [`Self::SymlinkEscape`], which is reserved for an escape
    /// through an actual symbolic link. This covers a path that names its way
    /// out, and an escape the kernel blocked before any link was followed.
    OutsideWorkspace,
    /// A path traverses a symbolic link.
    SymlinkEscape,
    /// A byte, entry, or output limit was exceeded.
    ResourceLimit,
    /// An expected workspace file state did not match exactly.
    Conflict,
    /// Publication may have started and exact recovery is required before retry.
    RecoveryRequired,
    /// The environment or fake policy denied the requested process intent.
    Denied,
    /// Cooperative cancellation completed and the owned process group joined.
    Cancelled,
    /// The execution deadline elapsed and the owned process group joined.
    TimedOut,
    /// Cleanup reached its deadline without proving every owned resource joined.
    CleanupUnresolved,
    /// The backend cannot safely support the requested operation.
    Unsupported,
    /// A contained filesystem operation failed.
    Io,
}

impl ExecutionBackendErrorKind {
    /// The disposition a category carries by default.
    ///
    /// A malformed request, a path or policy refusal, a bound, a foreign
    /// identity, an expectation that did not hold, and an operation the
    /// backend does not support are *usually* decided on the way in, before a
    /// process is spawned or a byte is written, so they default to no-effect.
    /// The remaining categories can follow work that may already have
    /// happened, so they default to undetermined.
    ///
    /// **This is only the default.** A category cannot see its own raise site:
    /// a bound checked before a spawn and a bound noticed after a child has
    /// already run are both `ResourceLimit`. A site that knows better says so
    /// with [`ExecutionBackendError::with_proven_no_effect`], and that is what
    /// callers read.
    #[must_use]
    pub const fn proven_no_effect(self) -> bool {
        match self {
            Self::IdentityMismatch
            | Self::InvalidRequest
            | Self::WorkspacePolicy
            // Each of these is decided by looking, before anything is opened
            // for writing or a byte moves.
            | Self::NotFound
            | Self::IsADirectory
            | Self::NotARegularFile
            | Self::OutsideWorkspace
            | Self::SymlinkEscape
            | Self::ResourceLimit
            | Self::Conflict
            | Self::Denied
            | Self::Unsupported => true,
            Self::RecoveryRequired
            | Self::Cancelled
            | Self::TimedOut
            | Self::CleanupUnresolved
            | Self::Io => false,
        }
    }
}

/// Bounded, content-safe backend error.
#[derive(Clone, Eq, PartialEq)]
pub struct ExecutionBackendError {
    kind: ExecutionBackendErrorKind,
    message: SafeDiagnostic,
    proven_no_effect: bool,
}

impl ExecutionBackendError {
    fn new(kind: ExecutionBackendErrorKind, message: impl AsRef<str>) -> Self {
        Self {
            kind,
            message: SafeDiagnostic::new(message),
            proven_no_effect: kind.proven_no_effect(),
        }
    }

    /// Records whether this error was raised before the backend had an effect.
    ///
    /// The raise site knows what its category cannot: whether a child had
    /// already run by the time the bound was noticed.
    #[must_use]
    pub const fn with_proven_no_effect(mut self, proven: bool) -> Self {
        self.proven_no_effect = proven;
        self
    }

    /// Reports whether this error is known to have been raised before the
    /// backend had any effect.
    #[must_use]
    pub const fn proven_no_effect(&self) -> bool {
        self.proven_no_effect
    }

    /// Returns the stable failure category.
    #[must_use]
    pub const fn kind(&self) -> ExecutionBackendErrorKind {
        self.kind
    }

    /// Returns the bounded safe diagnostic.
    #[must_use]
    pub const fn message(&self) -> &SafeDiagnostic {
        &self.message
    }
}

impl fmt::Debug for ExecutionBackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutionBackendError")
            .field("kind", &self.kind)
            .field("message", &self.message)
            .field("proven_no_effect", &self.proven_no_effect)
            .finish()
    }
}

impl fmt::Display for ExecutionBackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(formatter)
    }
}

impl std::error::Error for ExecutionBackendError {}

/// Cooperative control shared by all environment operations.
#[derive(Clone, Debug)]
pub struct ExecutionOperationControl {
    cancellation: CancellationToken,
    preempt: tokio::sync::watch::Receiver<crate::ToolPreempt>,
}

impl ExecutionOperationControl {
    /// Creates controls for one operation.
    #[must_use]
    pub fn new(cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            preempt: tokio::sync::watch::channel(crate::ToolPreempt::None).1,
        }
    }

    /// Binds a yield-only control signal for process promotion.
    #[must_use]
    pub fn with_preempt(
        mut self,
        preempt: tokio::sync::watch::Receiver<crate::ToolPreempt>,
    ) -> Self {
        self.preempt = preempt;
        self
    }

    /// Returns the yield request independently from the kill token.
    #[must_use]
    pub const fn preempt(&self) -> &tokio::sync::watch::Receiver<crate::ToolPreempt> {
        &self.preempt
    }

    /// Returns cooperative cancellation.
    #[must_use]
    pub const fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// Refuses once the operation's cancellation token is observed.
    ///
    /// # Errors
    ///
    /// Returns a cancelled backend error when the token has fired.
    pub fn check_cancelled(&self) -> Result<(), ExecutionBackendError> {
        if self.cancellation.is_cancelled() {
            Err(backend_error(
                ExecutionBackendErrorKind::Cancelled,
                "execution environment operation was cancelled",
            ))
        } else {
            Ok(())
        }
    }
}

/// Redacted environment value resolved for one exact delegation binding.
#[derive(Clone)]
pub struct EnvironmentVariableValue(Arc<str>);

impl fmt::Debug for EnvironmentVariableValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("EnvironmentVariableValue(<redacted>)")
    }
}

/// Exact workspace authority supplied to a sealed backend.
#[derive(Clone)]
pub struct ExecutionWorkspace {
    root: Arc<PathBuf>,
    cwd: Arc<PathBuf>,
    root_device: u64,
    root_inode: u64,
}

impl ExecutionWorkspace {
    fn from_environment(environment: &ExecutionEnvironment) -> Self {
        Self {
            root: Arc::new(environment.root().to_path_buf()),
            cwd: Arc::new(environment.cwd().to_path_buf()),
            root_device: environment.descriptor().root_object().device(),
            root_inode: environment.descriptor().root_object().inode(),
        }
    }

    /// Returns the canonical workspace root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the canonical workspace cwd.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
}

impl fmt::Debug for ExecutionWorkspace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutionWorkspace")
            .field("root", &"<redacted>")
            .field("cwd", &"<redacted>")
            .finish()
    }
}

/// Exact process request after environment allowlist resolution.
#[derive(Clone)]
pub struct PreparedLiteralProcess {
    request: LiteralProcessRequest,
    environment: Arc<[(EnvironmentVariableName, EnvironmentVariableValue)]>,
}

impl PreparedLiteralProcess {
    /// Returns the original literal request.
    #[must_use]
    pub const fn request(&self) -> &LiteralProcessRequest {
        &self.request
    }

    fn environment_names(&self) -> Vec<EnvironmentVariableName> {
        self.environment
            .iter()
            .map(|(name, _)| name.clone())
            .collect()
    }
}

impl fmt::Debug for PreparedLiteralProcess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let environment_bytes = self
            .environment
            .iter()
            .map(|(_, value)| value.0.len())
            .sum::<usize>();
        formatter
            .debug_struct("PreparedLiteralProcess")
            .field("request", &self.request)
            .field("environment_count", &self.environment.len())
            .field("environment_bytes", &environment_bytes)
            .field("environment", &"<redacted>")
            .finish()
    }
}

mod sealed {
    pub trait Sealed {}
}

/// Object-safe owned-future execution authority.
///
/// The trait is sealed so a deserialized identity cannot be wrapped in an
/// attacker-provided backend and converted into live authority.
pub trait ExecutionBackend: sealed::Sealed + Send + Sync + 'static {
    /// Returns immutable backend identity.
    fn identity(&self) -> &ExecutionBackendIdentity;

    /// Resolves a value only for one exact configured non-credential binding.
    fn resolve_environment(
        &self,
        binding: &EnvironmentDelegation,
    ) -> Option<EnvironmentVariableValue>;

    /// Reads one bounded contained regular file.
    fn read_workspace(
        &self,
        workspace: ExecutionWorkspace,
        request: WorkspaceReadSliceRequest,
        control: ExecutionOperationControl,
    ) -> ExecutionBackendFuture<WorkspaceReadSlice>;

    /// Runs one bounded deterministic repository query without following symlinks.
    fn query_repository(
        &self,
        workspace: ExecutionWorkspace,
        query: RepositoryQuery,
        control: ExecutionOperationControl,
    ) -> ExecutionBackendFuture<RepositoryQueryResult>;

    /// Preflights and publishes one bounded workspace mutation.
    fn mutate_workspace(
        &self,
        workspace: ExecutionWorkspace,
        transaction: WorkspaceMutationTransaction,
        control: ExecutionOperationControl,
    ) -> ExecutionBackendFuture<WorkspaceMutationTransactionResult>;

    /// Executes or simulates one literal process request.
    fn execute_literal_process(
        &self,
        workspace: ExecutionWorkspace,
        request: PreparedLiteralProcess,
        limits: ExecutionEnvironmentLimits,
        control: ExecutionOperationControl,
    ) -> ExecutionBackendFuture<LiteralProcessResult>;
}

/// Truthful host backend with bounded filesystem and Unix process access.
///
/// Process children run with the embedding process's full host authority.
/// Literal process execution owns a fresh Unix process group: a child that
/// deliberately leaves that group also leaves its cleanup authority.
/// Unsupported targets fail closed.
pub struct HostExecutionBackend {
    identity: ExecutionBackendIdentity,
    process: Arc<HostProcessContext>,
    environment: BTreeMap<EnvironmentDelegation, EnvironmentVariableValue>,
    mutation_lock: Arc<Mutex<()>>,
    workspace_snapshots: Arc<Mutex<WorkspaceSnapshotCache>>,
}

impl HostExecutionBackend {
    pub(super) fn new(
        config: &ExecutionEnvironmentConfig,
        process: Arc<HostProcessContext>,
    ) -> Result<Self, ExecutionEnvironmentError> {
        let mut environment = BTreeMap::new();
        for binding in config.environment_delegations() {
            if !binding.is_host_snapshot() {
                return Err(ExecutionEnvironmentError::new(
                    ExecutionEnvironmentErrorKind::InvalidDelegationSourceAuthority,
                    "host environment delegation does not name the host snapshot source",
                ));
            }
            if let Ok(value) = std::env::var(binding.name().as_str())
                && value.len() <= MAX_EXECUTION_VALUE_BYTES
                && !value.as_bytes().contains(&0)
            {
                environment.insert(binding.clone(), EnvironmentVariableValue(value.into()));
            }
        }
        Ok(Self {
            identity: ExecutionBackendIdentity::current_host()?,
            process,
            environment,
            mutation_lock: Arc::new(Mutex::new(())),
            workspace_snapshots: Arc::new(Mutex::new(WorkspaceSnapshotCache::default())),
        })
    }
}

impl sealed::Sealed for HostExecutionBackend {}

impl ExecutionBackend for HostExecutionBackend {
    fn identity(&self) -> &ExecutionBackendIdentity {
        &self.identity
    }

    fn resolve_environment(
        &self,
        binding: &EnvironmentDelegation,
    ) -> Option<EnvironmentVariableValue> {
        self.environment.get(binding).cloned()
    }

    fn read_workspace(
        &self,
        workspace: ExecutionWorkspace,
        request: WorkspaceReadSliceRequest,
        control: ExecutionOperationControl,
    ) -> ExecutionBackendFuture<WorkspaceReadSlice> {
        let snapshots = Arc::clone(&self.workspace_snapshots);
        run_filesystem_blocking(move || {
            read_workspace_sync(&workspace, &request, &control, &snapshots)
        })
    }

    fn query_repository(
        &self,
        workspace: ExecutionWorkspace,
        query: RepositoryQuery,
        control: ExecutionOperationControl,
    ) -> ExecutionBackendFuture<RepositoryQueryResult> {
        run_filesystem_blocking(move || query_repository_sync(&workspace, &query, &control))
    }

    fn mutate_workspace(
        &self,
        workspace: ExecutionWorkspace,
        transaction: WorkspaceMutationTransaction,
        control: ExecutionOperationControl,
    ) -> ExecutionBackendFuture<WorkspaceMutationTransactionResult> {
        let mutation_lock = Arc::clone(&self.mutation_lock);
        run_filesystem_blocking(move || {
            let _guard = lock(&mutation_lock);
            mutate_workspace_sync(&workspace, &transaction, &control)
        })
    }

    fn execute_literal_process(
        &self,
        workspace: ExecutionWorkspace,
        request: PreparedLiteralProcess,
        limits: ExecutionEnvironmentLimits,
        control: ExecutionOperationControl,
    ) -> ExecutionBackendFuture<LiteralProcessResult> {
        execute_host_literal_process(
            workspace,
            request,
            limits,
            control,
            Arc::clone(&self.process),
        )
    }
}

impl ExecutionEnvironment {
    /// Reads one slice of a job output file this environment's process service
    /// issued. A path lexically under the bound jobs root that this service
    /// does not hold issued is refused without I/O. Returns `None` for every
    /// other path, which stays under workspace containment.
    #[must_use]
    pub fn read_job_output(
        &self,
        path: &str,
        offset: u64,
        max_bytes: u64,
    ) -> Option<ExecutionBackendFuture<JobOutputSlice>> {
        match self
            .existing_joined_process_sessions()?
            .admit_job_output(path)?
        {
            job_output::JobOutputAdmission::Issued(location) => {
                Some(run_filesystem_blocking(move || {
                    location.read(offset, max_bytes)
                }))
            }
            job_output::JobOutputAdmission::Refused(error) => {
                Some(Box::pin(std::future::ready(Err(error))))
            }
        }
    }

    /// Reads one bounded regular file through the exact live backend.
    pub fn read_workspace(
        &self,
        request: WorkspaceReadSliceRequest,
        control: ExecutionOperationControl,
    ) -> ExecutionBackendFuture<WorkspaceReadSlice> {
        if let Err(error) = self.verify_operation_backend() {
            return ready_error(error);
        }
        if let Err(error) = control.check_cancelled() {
            return ready_error(error);
        }
        if let Err(error) = request.enforce_limit(self.config().limits().max_artifact_bytes()) {
            return ready_error(error);
        }
        self.operation_backend.read_workspace(
            ExecutionWorkspace::from_environment(self),
            request,
            control,
        )
    }

    /// Runs one bounded deterministic repository query without following symlinks.
    pub fn query_repository(
        &self,
        query: RepositoryQuery,
        control: ExecutionOperationControl,
    ) -> ExecutionBackendFuture<RepositoryQueryResult> {
        if let Err(error) = self.verify_operation_backend() {
            return ready_error(error);
        }
        if let Err(error) = control.check_cancelled() {
            return ready_error(error);
        }
        // Scanning many files does not retain their aggregate bytes as an artifact.
        if query.limits().max_file_bytes > self.config().limits().max_artifact_bytes()
            || u64::try_from(query.limits().max_result_bytes).unwrap_or(u64::MAX)
                > self.config().limits().max_artifact_bytes()
        {
            return ready_error(resource_error(
                "repository query exceeds environment artifact limits",
            ));
        }
        self.operation_backend.query_repository(
            ExecutionWorkspace::from_environment(self),
            query,
            control,
        )
    }

    /// Preflights and publishes one bounded workspace mutation through the exact
    /// live backend.
    ///
    /// Cancellation is observed before publication starts. Once normal mutation
    /// begins, the operation finishes its bounded request so callers never
    /// receive a safe `Cancelled` result for changes that may already exist.
    pub fn mutate_workspace(
        &self,
        transaction: WorkspaceMutationTransaction,
        control: ExecutionOperationControl,
    ) -> ExecutionBackendFuture<WorkspaceMutationTransactionResult> {
        if let Err(error) = self.verify_operation_backend() {
            return ready_error(error);
        }
        if let Err(error) = control.check_cancelled() {
            return ready_error(error);
        }
        if let Err(error) = transaction.enforce_limit(self.config().limits().max_artifact_bytes()) {
            return ready_error(error);
        }
        self.operation_backend.mutate_workspace(
            ExecutionWorkspace::from_environment(self),
            transaction,
            control,
        )
    }

    /// Executes one literal process request through the exact live backend.
    ///
    /// Only complete configured environment delegations are resolved. Credential
    /// delegations never enter the prepared child environment.
    pub fn execute_literal_process(
        &self,
        request: LiteralProcessRequest,
        control: ExecutionOperationControl,
    ) -> ExecutionBackendFuture<LiteralProcessResult> {
        if let Err(error) = self.verify_operation_backend() {
            return ready_error(error);
        }
        if let Err(error) = control.check_cancelled() {
            return ready_error(error);
        }
        let environment = self
            .config()
            .environment_delegations()
            .iter()
            .filter_map(|binding| {
                self.operation_backend
                    .resolve_environment(binding)
                    .map(|value| (binding.name().clone(), value))
            })
            .collect::<Vec<_>>();
        let prepared = PreparedLiteralProcess {
            request,
            environment: environment.into(),
        };
        let future = self.operation_backend.execute_literal_process(
            ExecutionWorkspace::from_environment(self),
            prepared,
            self.config().limits(),
            control.clone(),
        );
        let limits = self.config().limits();
        Box::pin(async move {
            let result = future.await?;
            control.check_cancelled()?;
            enforce_process_output_limits(&result, limits)?;
            Ok(result)
        })
    }

    fn verify_operation_backend(&self) -> Result<(), ExecutionBackendError> {
        if self.operation_backend.identity() == self.backend()
            && self.matches_descriptor(self.descriptor())
        {
            Ok(())
        } else {
            Err(backend_error(
                ExecutionBackendErrorKind::IdentityMismatch,
                "execution environment backend authority no longer matches",
            ))
        }
    }
}

fn enforce_process_output_limits(
    result: &LiteralProcessResult,
    limits: ExecutionEnvironmentLimits,
) -> Result<(), ExecutionBackendError> {
    let stdout = u64::try_from(result.stdout().len()).unwrap_or(u64::MAX);
    let stderr = u64::try_from(result.stderr().len()).unwrap_or(u64::MAX);
    if stdout > limits.max_stdout_bytes()
        || stderr > limits.max_stderr_bytes()
        || stdout.saturating_add(stderr) > limits.max_combined_output_bytes()
    {
        return Err(resource_error(
            "literal process output exceeds environment limits",
        ));
    }
    Ok(())
}

fn validate_relative_path(path: &Path, allow_empty: bool) -> Result<(), ExecutionBackendError> {
    if path.as_os_str().is_empty() {
        if allow_empty {
            return Ok(());
        }
        return Err(invalid_request("execution path cannot be empty"));
    }
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(backend_error(
            ExecutionBackendErrorKind::OutsideWorkspace,
            format!("path is outside the workspace: {}", path.display()),
        ));
    }
    validate_path_bytes(path)
}

fn validate_workspace_byte_limit(
    max_bytes: u64,
    message: &'static str,
) -> Result<(), ExecutionBackendError> {
    if max_bytes == 0 || max_bytes > MAX_ENVIRONMENT_OUTPUT_BYTES {
        Err(invalid_request(message))
    } else {
        Ok(())
    }
}

fn normalize_relative_path(
    path: &Path,
    allow_empty: bool,
) -> Result<PathBuf, ExecutionBackendError> {
    validate_relative_path(path, allow_empty)?;
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(backend_error(
                    ExecutionBackendErrorKind::OutsideWorkspace,
                    format!("path is outside the workspace: {}", path.display()),
                ));
            }
        }
    }
    if normalized.as_os_str().is_empty() && !allow_empty {
        return Err(invalid_request("execution path cannot be empty"));
    }
    validate_path_bytes(&normalized)?;
    Ok(normalized)
}

fn validate_path_bytes(path: &Path) -> Result<(), ExecutionBackendError> {
    let text = path
        .to_str()
        .ok_or_else(|| invalid_request("execution path is not valid UTF-8"))?;
    if text.len() > MAX_EXECUTION_VALUE_BYTES || text.as_bytes().contains(&0) {
        return Err(invalid_request("execution path exceeds its byte limit"));
    }
    Ok(())
}

fn validate_literal(value: &str, message: &'static str) -> Result<(), ExecutionBackendError> {
    if value.is_empty() || value.len() > MAX_EXECUTION_VALUE_BYTES || value.as_bytes().contains(&0)
    {
        Err(invalid_request(message))
    } else {
        Ok(())
    }
}

fn ready_error<T: Send + 'static>(error: ExecutionBackendError) -> ExecutionBackendFuture<T> {
    Box::pin(async move { Err(error) })
}

fn backend_error(
    kind: ExecutionBackendErrorKind,
    message: impl AsRef<str>,
) -> ExecutionBackendError {
    ExecutionBackendError::new(kind, message)
}

fn invalid_request(message: &'static str) -> ExecutionBackendError {
    backend_error(ExecutionBackendErrorKind::InvalidRequest, message)
}

fn resource_error(message: &'static str) -> ExecutionBackendError {
    backend_error(ExecutionBackendErrorKind::ResourceLimit, message)
}

/// Canonical refusal for input to a joined process whose stdin is closed;
/// unified-exec projections replace it with retry guidance.
pub(crate) const JOINED_STDIN_CLOSED: &str = "joined process stdin is closed";

fn conflict_error(message: &'static str) -> ExecutionBackendError {
    backend_error(ExecutionBackendErrorKind::Conflict, message)
}

fn recovery_required_error(message: &'static str) -> ExecutionBackendError {
    backend_error(ExecutionBackendErrorKind::RecoveryRequired, message)
}

fn workspace_error(message: &'static str) -> ExecutionBackendError {
    backend_error(ExecutionBackendErrorKind::WorkspacePolicy, message)
}

fn io_error(message: &'static str) -> ExecutionBackendError {
    backend_error(ExecutionBackendErrorKind::Io, message)
}

fn cleanup_unresolved_error() -> ExecutionBackendError {
    backend_error(
        ExecutionBackendErrorKind::CleanupUnresolved,
        "joined process cleanup did not prove every owned resource joined",
    )
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
