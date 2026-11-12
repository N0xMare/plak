//! Fixed, bounded operator VCS reads, shared by local and paired execution.

#![allow(
    missing_docs,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    reason = "unsupported operator execution seam, documented in spec 04 section 2.6"
)]

mod types;
pub use types::*;
#[cfg(unix)]
mod git;
#[cfg(unix)]
mod process;
#[cfg(unix)]
mod repository;
#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;

use super::{ExecutionBackendFuture, OperationContext};
use crate::ExecutionEnvironment;

/// Executes on the environment's frozen backend, never an equal local pathname.
pub fn workspace_changes(
    environment: &ExecutionEnvironment,
    request: ChangesRequest,
    control: OperationContext,
) -> ExecutionBackendFuture<ChangesAnswer> {
    environment.workspace_changes(request, control)
}

#[cfg(unix)]
pub(super) fn execute(
    workspace: super::ExecutionWorkspace,
    request: ChangesRequest,
    control: OperationContext,
) -> ExecutionBackendFuture<ChangesAnswer> {
    Box::pin(async move {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let cancellation = control.cancellation().clone();
        let result = tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(ChangesRefusal::Cancelled),
            () = tokio::time::sleep_until(deadline) => Err(ChangesRefusal::Deadline),
            answer = git::execute(workspace, request, deadline, cancellation.clone()) => answer,
        };
        Ok(result.unwrap_or_else(ChangesAnswer::Refused))
    })
}
