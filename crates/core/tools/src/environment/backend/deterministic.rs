//! Deterministic execution backend responsibilities.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::{
    EnvironmentDelegation, EnvironmentVariableName, EnvironmentVariableValue, ExecutionBackend,
    ExecutionBackendErrorKind, ExecutionBackendFuture, ExecutionBackendIdentity,
    ExecutionEnvironmentError, ExecutionEnvironmentErrorKind, ExecutionEnvironmentLimits,
    ExecutionOperationControl, ExecutionWorkspace, LiteralProcessIntent, LiteralProcessResult,
    MAX_EXECUTION_VALUE_BYTES, PreparedLiteralProcess, RepositoryQuery, RepositoryQueryResult,
    WorkspaceMutationTransaction, WorkspaceMutationTransactionResult, WorkspaceReadSlice,
    WorkspaceReadSliceRequest, WorkspaceSnapshotCache, backend_error, lock, mutate_workspace_sync,
    query_repository_sync, read_workspace_sync, ready_error, resolve_existing_no_symlink,
    run_filesystem_blocking, sealed, workspace_error,
};

/// Immutable process permission simulation used by the fake backend.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeterministicProcessPolicy {
    allow_process: bool,
    allow_destructive: bool,
    allow_network: bool,
}

impl DeterministicProcessPolicy {
    /// Creates explicit fake-backend decisions.
    #[must_use]
    pub const fn new(allow_process: bool, allow_destructive: bool, allow_network: bool) -> Self {
        Self {
            allow_process,
            allow_destructive,
            allow_network,
        }
    }

    fn permits(self, intent: LiteralProcessIntent) -> bool {
        self.allow_process
            && (!intent.destructive() || self.allow_destructive)
            && (!intent.network() || self.allow_network)
    }
}

/// One deterministic process behavior consumed in FIFO order.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DeterministicProcessStep {
    /// Completes immediately with the supplied bounded result.
    Complete(LiteralProcessResult),
    /// Remains active until cancellation, then joins simulated descendants.
    WaitForCancellation {
        /// Number of simulated descendants owned by this process.
        descendants: usize,
    },
    /// Reports that process execution is unavailable.
    Unsupported,
}

/// Redacted capture proving literal argv and allowlisted environment names.
#[derive(Clone, Eq, PartialEq)]
pub struct RecordedLiteralProcess {
    program: Arc<str>,
    arguments: Arc<[String]>,
    cwd: PathBuf,
    stdin_bytes: usize,
    environment_names: Arc<[EnvironmentVariableName]>,
    intent: LiteralProcessIntent,
}

impl RecordedLiteralProcess {
    /// Returns exact program text.
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    /// Returns exact separate argv values.
    #[must_use]
    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }

    /// Returns the relative cwd.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Returns stdin byte count without exposing content.
    #[must_use]
    pub const fn stdin_bytes(&self) -> usize {
        self.stdin_bytes
    }

    /// Returns injected environment names without values.
    #[must_use]
    pub fn environment_names(&self) -> &[EnvironmentVariableName] {
        &self.environment_names
    }

    /// Returns declared effects.
    #[must_use]
    pub const fn intent(&self) -> LiteralProcessIntent {
        self.intent
    }
}

impl fmt::Debug for RecordedLiteralProcess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecordedLiteralProcess")
            .field("program_bytes", &self.program.len())
            .field("argument_count", &self.arguments.len())
            .field("cwd", &"<redacted>")
            .field("stdin_bytes", &self.stdin_bytes)
            .field("environment_count", &self.environment_names.len())
            .field("intent", &self.intent)
            .finish()
    }
}

/// Live-resource counts for deterministic cleanup assertions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeterministicBackendStatus {
    active_processes: usize,
    active_descendants: usize,
}

impl DeterministicBackendStatus {
    /// Returns active simulated direct processes.
    #[must_use]
    pub const fn active_processes(self) -> usize {
        self.active_processes
    }

    /// Returns active simulated descendants.
    #[must_use]
    pub const fn active_descendants(self) -> usize {
        self.active_descendants
    }

    /// Reports whether all owned process resources were joined.
    #[must_use]
    pub const fn is_drained(self) -> bool {
        self.active_processes == 0 && self.active_descendants == 0
    }
}

/// Injectable deterministic backend for effect-authority conformance.
pub struct DeterministicExecutionBackend {
    identity: ExecutionBackendIdentity,
    environment:
        BTreeMap<EnvironmentVariableName, (EnvironmentDelegation, EnvironmentVariableValue)>,
    process_policy: DeterministicProcessPolicy,
    process_steps: Mutex<VecDeque<DeterministicProcessStep>>,
    process_calls: Mutex<Vec<RecordedLiteralProcess>>,
    active_processes: Arc<AtomicUsize>,
    active_descendants: Arc<AtomicUsize>,
    mutation_lock: Arc<Mutex<()>>,
    workspace_snapshots: Arc<Mutex<WorkspaceSnapshotCache>>,
}

impl DeterministicExecutionBackend {
    /// Creates one exact fake authority with FIFO process steps.
    ///
    /// Environment values are bound to complete delegation identities; extra
    /// values are never injected unless the environment config selects the same
    /// exact binding.
    ///
    /// # Errors
    ///
    /// Rejects duplicate bindings, invalid/NUL values, or identity mint failure.
    pub fn new(
        environment: impl IntoIterator<Item = (EnvironmentDelegation, String)>,
        process_policy: DeterministicProcessPolicy,
        process_steps: impl IntoIterator<Item = DeterministicProcessStep>,
    ) -> Result<Self, ExecutionEnvironmentError> {
        let mut values = BTreeMap::new();
        for (binding, value) in environment {
            if value.len() > MAX_EXECUTION_VALUE_BYTES || value.as_bytes().contains(&0) {
                return Err(ExecutionEnvironmentError::new(
                    ExecutionEnvironmentErrorKind::InvalidEnvironmentVariable,
                    "deterministic environment value is invalid",
                ));
            }
            if values
                .insert(
                    binding.name().clone(),
                    (binding, EnvironmentVariableValue(value.into())),
                )
                .is_some()
            {
                return Err(ExecutionEnvironmentError::new(
                    ExecutionEnvironmentErrorKind::DuplicateAllowlistEntry,
                    "deterministic environment contains a duplicate name",
                ));
            }
        }
        Ok(Self {
            identity: ExecutionBackendIdentity::deterministic_fake()?,
            environment: values,
            process_policy,
            process_steps: Mutex::new(process_steps.into_iter().collect()),
            process_calls: Mutex::new(Vec::new()),
            active_processes: Arc::new(AtomicUsize::new(0)),
            active_descendants: Arc::new(AtomicUsize::new(0)),
            mutation_lock: Arc::new(Mutex::new(())),
            workspace_snapshots: Arc::new(Mutex::new(WorkspaceSnapshotCache::default())),
        })
    }

    /// Returns redacted process captures in dispatch order.
    #[must_use]
    pub fn process_calls(&self) -> Vec<RecordedLiteralProcess> {
        lock(&self.process_calls).clone()
    }

    /// Returns current simulated process-resource counts.
    #[must_use]
    pub fn status(&self) -> DeterministicBackendStatus {
        DeterministicBackendStatus {
            active_processes: self.active_processes.load(Ordering::Acquire),
            active_descendants: self.active_descendants.load(Ordering::Acquire),
        }
    }
}

impl fmt::Debug for DeterministicExecutionBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeterministicExecutionBackend")
            .field("identity", &self.identity)
            .field("environment_count", &self.environment.len())
            .field("environment", &"<redacted>")
            .field("process_policy", &self.process_policy)
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl sealed::Sealed for DeterministicExecutionBackend {}

impl ExecutionBackend for DeterministicExecutionBackend {
    fn identity(&self) -> &ExecutionBackendIdentity {
        &self.identity
    }

    fn resolve_environment(
        &self,
        binding: &EnvironmentDelegation,
    ) -> Option<EnvironmentVariableValue> {
        self.environment
            .get(binding.name())
            .filter(|(expected, _)| expected == binding)
            .map(|(_, value)| value.clone())
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
        _limits: ExecutionEnvironmentLimits,
        control: ExecutionOperationControl,
    ) -> ExecutionBackendFuture<LiteralProcessResult> {
        let resolved_cwd = match resolve_existing_no_symlink(&workspace, request.request.cwd()) {
            Ok(path) => path,
            Err(error) => return ready_error(error),
        };
        if !resolved_cwd.is_dir() {
            return ready_error(workspace_error(
                "literal process cwd is not a contained directory",
            ));
        }
        let permitted = self.process_policy.permits(request.request.intent());
        if !permitted {
            return Box::pin(async {
                Err(backend_error(
                    ExecutionBackendErrorKind::Denied,
                    "deterministic process policy denied the requested intent",
                ))
            });
        }
        let recorded = RecordedLiteralProcess {
            program: Arc::clone(&request.request.program),
            arguments: Arc::clone(&request.request.arguments),
            cwd: request.request.cwd.clone(),
            stdin_bytes: request.request.stdin.len(),
            environment_names: request.environment_names().into(),
            intent: request.request.intent,
        };
        lock(&self.process_calls).push(recorded);
        let step = lock(&self.process_steps).pop_front();
        let active = FakeProcessGuard::new(
            Arc::clone(&self.active_processes),
            Arc::clone(&self.active_descendants),
            match &step {
                Some(DeterministicProcessStep::WaitForCancellation { descendants }) => *descendants,
                _ => 0,
            },
        );
        match step {
            Some(DeterministicProcessStep::Complete(result)) => Box::pin(async move {
                let _active = active;
                control.check_cancelled()?;
                Ok(result)
            }),
            Some(DeterministicProcessStep::WaitForCancellation { .. }) => Box::pin(async move {
                let _active = active;
                if !control.cancellation().is_cancelled() {
                    control.cancellation().cancelled().await;
                }
                Err(backend_error(
                    ExecutionBackendErrorKind::Cancelled,
                    "deterministic process tree was cancelled and joined",
                ))
            }),
            Some(DeterministicProcessStep::Unsupported) | None => Box::pin(async move {
                let _active = active;
                Err(backend_error(
                    ExecutionBackendErrorKind::Unsupported,
                    "deterministic process script has no executable step",
                ))
            }),
        }
    }
}

struct FakeProcessGuard {
    processes: Arc<AtomicUsize>,
    descendants_counter: Arc<AtomicUsize>,
    descendants: usize,
}

impl FakeProcessGuard {
    fn new(
        processes: Arc<AtomicUsize>,
        descendants_counter: Arc<AtomicUsize>,
        descendants: usize,
    ) -> Self {
        processes.fetch_add(1, Ordering::AcqRel);
        descendants_counter.fetch_add(descendants, Ordering::AcqRel);
        Self {
            processes,
            descendants_counter,
            descendants,
        }
    }
}

impl Drop for FakeProcessGuard {
    fn drop(&mut self) {
        self.processes.fetch_sub(1, Ordering::AcqRel);
        self.descendants_counter
            .fetch_sub(self.descendants, Ordering::AcqRel);
    }
}
