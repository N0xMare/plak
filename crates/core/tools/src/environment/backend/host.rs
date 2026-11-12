//! The truthful host execution backend.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use super::{
    EnvironmentDelegation, EnvironmentVariableValue, ExecutionBackend, ExecutionBackendErrorKind,
    ExecutionBackendFuture, ExecutionBackendIdentity, ExecutionEnvironmentConfig,
    ExecutionEnvironmentError, ExecutionEnvironmentErrorKind, ExecutionEnvironmentLimits,
    ExecutionWorkspace, HostProcessContext, LiteralProcessResult, MAX_EXECUTION_VALUE_BYTES,
    OperationContext, PreparedLiteralProcess, PreparedServiceProcess, RepositoryQuery,
    RepositoryQueryResult, ServiceProcess, WorkspaceMutationTransaction,
    WorkspaceMutationTransactionResult, WorkspaceReadSlice, WorkspaceReadSliceRequest,
    WorkspaceSnapshotCache, backend_error, execute_host_literal_process, lock,
    mutate_workspace_sync, query_repository_sync, read_workspace_sync, ready_error,
    run_filesystem_blocking, sealed, spawn_host_service,
};

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
    fn new(
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
            identity: ExecutionBackendIdentity::local_runner()?,
            process,
            environment,
            mutation_lock: Arc::new(Mutex::new(())),
            workspace_snapshots: Arc::new(Mutex::new(WorkspaceSnapshotCache::default())),
        })
    }
}

impl sealed::Sealed for HostExecutionBackend {}

impl ExecutionBackend for HostExecutionBackend {
    fn workspace_changes(
        &self,
        workspace: ExecutionWorkspace,
        request: super::changes::ChangesRequest,
        control: OperationContext,
    ) -> ExecutionBackendFuture<super::changes::ChangesAnswer> {
        super::changes::execute(workspace, request, control)
    }

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
        control: OperationContext,
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
        control: OperationContext,
    ) -> ExecutionBackendFuture<RepositoryQueryResult> {
        run_filesystem_blocking(move || query_repository_sync(&workspace, &query, &control))
    }

    fn mutate_workspace(
        &self,
        workspace: ExecutionWorkspace,
        transaction: WorkspaceMutationTransaction,
        control: OperationContext,
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
        control: OperationContext,
    ) -> ExecutionBackendFuture<LiteralProcessResult> {
        execute_host_literal_process(
            workspace,
            request,
            limits,
            control,
            Arc::clone(&self.process),
        )
    }

    fn spawn_service(
        &self,
        workspace: ExecutionWorkspace,
        request: PreparedServiceProcess,
        control: OperationContext,
    ) -> ExecutionBackendFuture<ServiceProcess> {
        // Runner secrets live on a runner; this backend has none to read.
        if !request.request().runner_secrets().is_empty() {
            return ready_error(
                backend_error(
                    ExecutionBackendErrorKind::Unsupported,
                    "a local service has no runner secrets; resolve them before the spawn",
                )
                .with_proven_no_effect(true),
            );
        }
        spawn_host_service(workspace, request, control, Arc::clone(&self.process))
    }
}

/// The host backend over one captured host process context.
pub(in crate::environment) fn host_execution_backend(
    config: &ExecutionEnvironmentConfig,
    process: &Arc<HostProcessContext>,
) -> Result<Arc<dyn ExecutionBackend>, ExecutionEnvironmentError> {
    Ok(Arc::new(HostExecutionBackend::new(
        config,
        Arc::clone(process),
    )?))
}
