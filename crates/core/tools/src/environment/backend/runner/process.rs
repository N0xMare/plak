//! The runner a host process runs its sessions on, when it is not the local
//! runner. Unsupported: only a test route selects one until runner pairing
//! exists, and a process selects at most one, once, before its first session.

use std::sync::{Arc, OnceLock};

use super::{RunnerBackend, RunnerWorkspaceProbe};
use crate::{LocalWorkspaceProbe, WorkspaceProbe};

/// One selected runner: its backend, and the runtime its link runs on.
pub struct ProcessRunner {
    backend: Arc<RunnerBackend>,
    runtime: tokio::runtime::Handle,
}

impl ProcessRunner {
    /// The backend sessions run their operations on.
    #[must_use]
    pub fn backend(&self) -> Arc<RunnerBackend> {
        Arc::clone(&self.backend)
    }

    /// The multi-thread runtime the backend's link runs on.
    #[must_use]
    pub const fn runtime(&self) -> &tokio::runtime::Handle {
        &self.runtime
    }
}

impl std::fmt::Debug for ProcessRunner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProcessRunner")
            .finish_non_exhaustive()
    }
}

static PROCESS_RUNNER: OnceLock<ProcessRunner> = OnceLock::new();

/// Selects `backend` for every session this process constructs from now on.
/// `false` when a runner was already selected; the first selection stays.
pub fn select_process_runner(backend: Arc<RunnerBackend>, runtime: tokio::runtime::Handle) -> bool {
    PROCESS_RUNNER
        .set(ProcessRunner { backend, runtime })
        .is_ok()
}

/// The selected runner, when the process runs its sessions on one.
#[must_use]
pub fn process_runner() -> Option<&'static ProcessRunner> {
    PROCESS_RUNNER.get()
}

/// The probe of the workspaces sessions run in: the selected runner's, or
/// this process's own filesystem.
#[must_use]
pub fn workspace_probe() -> Arc<dyn WorkspaceProbe> {
    match process_runner() {
        Some(runner) => Arc::new(RunnerWorkspaceProbe::new(
            runner.backend.link(),
            runner.runtime.clone(),
        )),
        None => Arc::new(LocalWorkspaceProbe),
    }
}
