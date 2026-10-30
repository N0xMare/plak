use super::*;
use crate::ExecutionEnvironmentScope;
use plakous_model::RunId;
use std::str::FromStr;

struct ReadingDuringMutation {
    inner: Arc<dyn ExecutionBackend>,
    environment: Mutex<Option<ExecutionEnvironment>>,
}

impl sealed::Sealed for ReadingDuringMutation {}

impl ExecutionBackend for ReadingDuringMutation {
    fn identity(&self) -> &ExecutionBackendIdentity {
        self.inner.identity()
    }

    fn resolve_environment(
        &self,
        binding: &EnvironmentDelegation,
    ) -> Option<EnvironmentVariableValue> {
        self.inner.resolve_environment(binding)
    }

    fn read_workspace(
        &self,
        workspace: ExecutionWorkspace,
        request: WorkspaceReadSliceRequest,
        control: ExecutionOperationControl,
    ) -> ExecutionBackendFuture<WorkspaceReadSlice> {
        self.inner.read_workspace(workspace, request, control)
    }

    fn query_repository(
        &self,
        workspace: ExecutionWorkspace,
        query: RepositoryQuery,
        control: ExecutionOperationControl,
    ) -> ExecutionBackendFuture<RepositoryQueryResult> {
        self.inner.query_repository(workspace, query, control)
    }

    fn mutate_workspace(
        &self,
        workspace: ExecutionWorkspace,
        transaction: WorkspaceMutationTransaction,
        control: ExecutionOperationControl,
    ) -> ExecutionBackendFuture<WorkspaceMutationTransactionResult> {
        let environment = self
            .environment
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        let inner = self.inner.clone();
        Box::pin(async move {
            let environment = environment.ok_or_else(|| {
                backend_error(ExecutionBackendErrorKind::Io, "missing test environment")
            })?;
            let path = Path::new("target");
            let run = environment.descriptor().scope().run_id();
            let identity = WorkspaceContentIdentity::from_bytes(b"old")?;
            environment.record_text_read(run, path, identity);
            assert_eq!(environment.last_text_read(run, path), Some(identity));
            inner
                .mutate_workspace(workspace, transaction, control)
                .await
        })
    }

    fn execute_literal_process(
        &self,
        workspace: ExecutionWorkspace,
        request: PreparedLiteralProcess,
        limits: ExecutionEnvironmentLimits,
        control: ExecutionOperationControl,
    ) -> ExecutionBackendFuture<LiteralProcessResult> {
        self.inner
            .execute_literal_process(workspace, request, limits, control)
    }

    fn spawn_service(
        &self,
        workspace: ExecutionWorkspace,
        request: PreparedServiceProcess,
        control: ExecutionOperationControl,
    ) -> ExecutionBackendFuture<ServiceProcess> {
        self.inner.spawn_service(workspace, request, control)
    }
}

#[tokio::test]
async fn successful_mutation_consumes_a_read_recorded_inside_the_backend()
-> Result<(), Box<dyn std::error::Error>> {
    for delete in [false, true] {
        let root = tempfile::tempdir()?;
        let path = Path::new("target");
        std::fs::write(root.path().join(path), "old")?;
        let run = RunId::from_str("00000000-0000-0000-0000-000000000041")?;
        let mut environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(run),
            root.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?;
        let backend = Arc::new(ReadingDuringMutation {
            inner: environment.operation_backend.clone(),
            environment: Mutex::new(None),
        });
        environment.operation_backend = backend.clone();
        *backend
            .environment
            .lock()
            .map_err(|_| "poisoned test environment")? = Some(environment.clone());
        let replacement = if delete {
            WorkspaceReplacement::Missing
        } else {
            WorkspaceReplacement::content(b"new".to_vec())
        };
        environment
            .mutate_workspace(
                WorkspaceMutationTransaction::new(vec![WorkspaceMutation::new(
                    path,
                    WorkspaceExpectedFileState::Content(WorkspaceContentIdentity::from_bytes(
                        b"old",
                    )?),
                    replacement,
                )?])?,
                ExecutionOperationControl::new(CancellationToken::new()),
            )
            .await?;
        if delete {
            std::fs::write(root.path().join(path), "old")?;
        }
        assert_eq!(environment.last_text_read(run, path), None);
    }
    Ok(())
}
