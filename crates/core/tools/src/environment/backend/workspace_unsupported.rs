//! Workspace operations on a target without a host filesystem.
//!
//! Each refuses with `Unsupported` and no effect.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::{
    ExecutionBackendError, ExecutionBackendErrorKind, ExecutionWorkspace, OperationContext,
    RepositoryQuery, RepositoryQueryResult, WorkspaceMutationTransaction,
    WorkspaceMutationTransactionResult, WorkspaceReadSlice, WorkspaceReadSliceRequest,
    backend_error,
};

#[derive(Default)]
pub(super) struct WorkspaceSnapshotCache;

fn unsupported() -> ExecutionBackendError {
    backend_error(
        ExecutionBackendErrorKind::Unsupported,
        "workspace files need a unix host",
    )
    .with_proven_no_effect(true)
}

pub(super) fn read_workspace_sync(
    _workspace: &ExecutionWorkspace,
    _request: &WorkspaceReadSliceRequest,
    _control: &OperationContext,
    _snapshots: &Mutex<WorkspaceSnapshotCache>,
) -> Result<WorkspaceReadSlice, ExecutionBackendError> {
    Err(unsupported())
}

pub(super) fn query_repository_sync(
    _workspace: &ExecutionWorkspace,
    _query: &RepositoryQuery,
    _control: &OperationContext,
) -> Result<RepositoryQueryResult, ExecutionBackendError> {
    Err(unsupported())
}

pub(super) fn mutate_workspace_sync(
    _workspace: &ExecutionWorkspace,
    _transaction: &WorkspaceMutationTransaction,
    _control: &OperationContext,
) -> Result<WorkspaceMutationTransactionResult, ExecutionBackendError> {
    Err(unsupported())
}

pub(super) fn resolve_existing_no_symlink(
    _workspace: &ExecutionWorkspace,
    _relative: &Path,
) -> Result<PathBuf, ExecutionBackendError> {
    Err(unsupported())
}
