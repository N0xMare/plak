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

#[cfg(unix)]
pub(super) struct ChangesEnvironment {
    pub path: std::ffi::OsString,
    pub home: Option<std::ffi::OsString>,
    pub product_home: Option<std::ffi::OsString>,
    pub state_home: Option<std::ffi::OsString>,
}

/// Executes on the environment's frozen backend, never an equal local pathname.
/// The caller supplies its compiled Git CLI gate, never a client-selected value.
pub fn workspace_changes(
    environment: &ExecutionEnvironment,
    request: ChangesRequest,
    control: OperationContext,
    git_cli_enabled: bool,
) -> ExecutionBackendFuture<ChangesAnswer> {
    if !git_cli_enabled {
        return Box::pin(std::future::ready(Ok(ChangesAnswer::Refused(
            ChangesRefusal::VcsUnavailable,
        ))));
    }
    environment.workspace_changes(request, control)
}

#[cfg(unix)]
pub(super) fn execute(
    workspace: super::ExecutionWorkspace,
    request: ChangesRequest,
    control: OperationContext,
    machine: ChangesEnvironment,
) -> ExecutionBackendFuture<ChangesAnswer> {
    Box::pin(async move {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let cancellation = control.cancellation().clone();
        let result = tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(ChangesRefusal::Cancelled),
            () = tokio::time::sleep_until(deadline) => Err(ChangesRefusal::Deadline),
            answer = git::execute(workspace, request, deadline, cancellation.clone(), machine) => answer,
        };
        Ok(result.unwrap_or_else(ChangesAnswer::Refused))
    })
}
