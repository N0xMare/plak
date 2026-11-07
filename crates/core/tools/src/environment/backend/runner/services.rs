//! Explicit runner backend and executor, shared without selecting process authority.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{RunnerBackend, RunnerWorkspaceProbe};
use crate::WorkspaceProbe;

/// One selected runner: its backend, and the runtime its link runs on.
#[derive(Clone)]
pub struct RunnerServices {
    backend: Arc<RunnerBackend>,
    runtime: tokio::runtime::Handle,
    available: Arc<AtomicBool>,
}

impl RunnerServices {
    /// Explicit services of one runner; construction selects no process global.
    #[must_use]
    pub fn new(backend: Arc<RunnerBackend>, runtime: tokio::runtime::Handle) -> Self {
        let available = Arc::new(AtomicBool::new(true));
        let link = Arc::new(AdmittedLink {
            inner: backend.link(),
            available: available.clone(),
        });
        let mut backend = (*backend).clone().running_on(runtime.clone());
        backend.link = link;
        Self {
            backend: Arc::new(backend),
            runtime,
            available,
        }
    }

    /// Gates new node calls while reconnect reconciliation/configuration is
    /// blocked. Already admitted futures retain their keyed recovery owner.
    pub fn set_available(&self, available: bool) {
        self.available.store(available, Ordering::Release);
    }

    /// Whether the owner has opened the new-call admission barrier.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.available.load(Ordering::Acquire)
    }

    /// Whether a built/open node still retains these services.
    #[must_use]
    pub fn has_node_users(&self) -> bool {
        Arc::strong_count(&self.backend) > 1
    }

    /// Workspace probe of this runner, never the host filesystem.
    #[must_use]
    pub fn workspace_probe(&self) -> Arc<dyn WorkspaceProbe> {
        Arc::new(RunnerWorkspaceProbe::new(
            self.backend.link(),
            self.runtime.clone(),
        ))
    }
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

struct AdmittedLink {
    inner: Arc<dyn super::RunnerLink>,
    available: Arc<AtomicBool>,
}

impl super::RunnerLink for AdmittedLink {
    fn call(&self, call: super::RunnerCall) -> super::RunnerCallFuture {
        if !self.available.load(Ordering::Acquire) {
            return Box::pin(std::future::ready(Err(super::RunnerFailure::Unavailable(
                "runner is blocked before dispatch".into(),
            ))));
        }
        self.inner.call(call)
    }

    fn open(&self, call: super::RunnerCall) -> super::RunnerOpenFuture {
        if !self.available.load(Ordering::Acquire) {
            return Box::pin(std::future::ready(Err(super::RunnerFailure::Unavailable(
                "runner is blocked before dispatch".into(),
            ))));
        }
        self.inner.open(call)
    }
}

impl std::fmt::Debug for RunnerServices {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RunnerServices")
            .finish_non_exhaustive()
    }
}
