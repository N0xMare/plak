//! The process-session service of one job owner, as a [`crate::JobHost`] holds it.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use crate::environment::runner::RunnerSessions;
use crate::{
    ExecutionBackendError, JobOutputRoot, JobStatus, JobStopReason, JobView,
    JoinedProcessHostCapacity, JoinedProcessSessionId, JoinedProcessSessions, JoinedProcessStatus,
};

/// One job owner's process-session service, as a host registry holds it.
///
/// A [`crate::JobHost`] stores, returns and transfers ports, never the
/// concrete service, so a service that lives with a remote runner can stand
/// behind the same handle. A port is attached for one [`crate::JobOwner`]; a
/// registry finds it again by that owner's line, or by the session and branch
/// of the head that currently owns it. A local port is the in-process
/// [`JoinedProcessSessions`] of the host backend, and each operation has that
/// service's exact semantics.
///
/// A runner port (unsupported, [`Self::runner`]) holds one line's sessions on
/// a runner. Its read accessors ([`Self::jobs`], the counts and flags, the
/// terminal generation and its change watch, [`Self::terminal_jobs_since`],
/// [`Self::session_for_number`] and [`Self::leftover`]) answer from the
/// host-side mirror the line's last job read left. The effects
/// ([`Self::stop_with_grace`], [`Self::stop_jobs`] and
/// [`Self::disown_leftovers`]) are sent to the runner and wait for its answer
/// on a thread of their own, as every caller of this synchronous port already
/// runs them off the async workers. The runner keeps its own capacity and job
/// outputs, so [`Self::bind_host_capacity`] and [`Self::bind_job_outputs`] do
/// nothing for it.
#[derive(Clone, Debug)]
pub struct JoinedSessionsPort {
    service: Service,
}

#[derive(Clone, Debug)]
enum Service {
    Local(JoinedProcessSessions),
    Runner(Arc<RunnerSessions>, tokio::runtime::Handle),
}

impl JoinedSessionsPort {
    /// The port of an in-process service.
    #[must_use]
    pub const fn local(sessions: JoinedProcessSessions) -> Self {
        Self {
            service: Service::Local(sessions),
        }
    }

    /// The port of one line's sessions on a runner, whose effects run on
    /// `runtime`. Unsupported: nothing selects it yet.
    #[doc(hidden)]
    #[must_use]
    pub fn runner(sessions: Arc<RunnerSessions>, runtime: tokio::runtime::Handle) -> Self {
        Self {
            service: Service::Runner(sessions, runtime),
        }
    }

    /// The in-process service behind this port, when it is one. An
    /// environment adopts only a service of its own process.
    #[must_use]
    pub const fn as_local(&self) -> Option<&JoinedProcessSessions> {
        match &self.service {
            Service::Local(sessions) => Some(sessions),
            Service::Runner(..) => None,
        }
    }

    /// See [`JoinedProcessSessions::bind_host_capacity`].
    pub fn bind_host_capacity(&self, capacity: JoinedProcessHostCapacity) {
        if let Service::Local(sessions) = &self.service {
            sessions.bind_host_capacity(capacity);
        }
    }

    /// See [`JoinedProcessSessions::bind_job_outputs`].
    pub fn bind_job_outputs(
        &self,
        root: JobOutputRoot,
        session: plakous_model::SessionId,
        branch: plakous_model::BranchId,
    ) {
        if let Service::Local(sessions) = &self.service {
            sessions.bind_job_outputs(root, session, branch);
        }
    }

    /// See [`JoinedProcessSessions::observe_jobs`]. A runner port has no job
    /// events yet: its observer is never called.
    pub fn observe_jobs(
        &self,
        observer: impl Fn(JobView) -> bool + Send + Sync + 'static,
        has_room: impl Fn() -> bool + Send + Sync + 'static,
    ) {
        if let Service::Local(sessions) = &self.service {
            sessions.observe_jobs(observer, has_room);
        }
    }

    /// See [`JoinedProcessSessions::jobs`].
    #[must_use]
    pub fn jobs(&self) -> Vec<JobView> {
        match &self.service {
            Service::Local(sessions) => sessions.jobs(),
            Service::Runner(sessions, _) => sessions.cached_jobs(),
        }
    }

    /// See [`JoinedProcessSessions::running_job_count`].
    #[must_use]
    pub fn running_job_count(&self) -> usize {
        match &self.service {
            Service::Local(sessions) => sessions.running_job_count(),
            Service::Runner(sessions, _) => sessions
                .cached_jobs()
                .iter()
                .filter(|job| job.status == JobStatus::Running)
                .count(),
        }
    }

    /// See [`JoinedProcessSessions::has_jobs`].
    #[must_use]
    pub fn has_jobs(&self) -> bool {
        match &self.service {
            Service::Local(sessions) => sessions.has_jobs(),
            Service::Runner(sessions, _) => !sessions.cached_jobs().is_empty(),
        }
    }

    /// See [`JoinedProcessSessions::has_unreported_terminal_job`]. A runner
    /// port reports no terminal until job events carry them.
    #[must_use]
    pub fn has_unreported_terminal_job(&self) -> bool {
        match &self.service {
            Service::Local(sessions) => sessions.has_unreported_terminal_job(),
            Service::Runner(..) => false,
        }
    }

    /// See [`JoinedProcessSessions::job_admission_closed`].
    #[must_use]
    pub fn job_admission_closed(&self) -> bool {
        match &self.service {
            Service::Local(sessions) => sessions.job_admission_closed(),
            Service::Runner(..) => false,
        }
    }

    /// See [`JoinedProcessSessions::job_terminal_generation`].
    #[must_use]
    pub fn job_terminal_generation(&self) -> u64 {
        match &self.service {
            Service::Local(sessions) => sessions.job_terminal_generation(),
            Service::Runner(sessions, _) => *sessions.terminal_changes().borrow(),
        }
    }

    /// See [`JoinedProcessSessions::job_terminal_changes`].
    #[must_use]
    pub fn job_terminal_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        match &self.service {
            Service::Local(sessions) => sessions.job_terminal_changes(),
            Service::Runner(sessions, _) => sessions.terminal_changes(),
        }
    }

    /// See [`JoinedProcessSessions::terminal_jobs_since`].
    #[must_use]
    pub fn terminal_jobs_since(&self, generation: u64) -> Vec<String> {
        match &self.service {
            Service::Local(sessions) => sessions.terminal_jobs_since(generation),
            Service::Runner(..) => Vec::new(),
        }
    }

    /// See [`JoinedProcessSessions::session_for_number`].
    #[must_use]
    pub fn session_for_number(&self, number: u32) -> Option<JoinedProcessSessionId> {
        match &self.service {
            Service::Local(sessions) => sessions.session_for_number(number),
            Service::Runner(sessions, _) => sessions.session_for_number(number),
        }
    }

    /// See [`JoinedProcessSessions::leftover`].
    #[must_use]
    pub fn leftover(&self, id: JoinedProcessSessionId) -> Option<(Option<i32>, usize)> {
        match &self.service {
            Service::Local(sessions) => sessions.leftover(id),
            Service::Runner(..) => None,
        }
    }

    /// See [`JoinedProcessSessions::stop_with_grace`].
    ///
    /// # Errors
    /// Returns the service's stop error.
    pub fn stop_with_grace(
        &self,
        id: JoinedProcessSessionId,
        by: JobStopReason,
        grace: Duration,
    ) -> Result<JoinedProcessStatus, ExecutionBackendError> {
        match &self.service {
            Service::Local(sessions) => sessions.stop_with_grace(id, by, grace),
            Service::Runner(sessions, runtime) => {
                wait(runtime, sessions.stop_with_grace(id, by, grace))?
            }
        }
    }

    /// See [`JoinedProcessSessions::stop_jobs`].
    ///
    /// # Errors
    /// Returns the service's stop error.
    pub fn stop_jobs(
        &self,
        by: JobStopReason,
        grace: Duration,
    ) -> Result<(), ExecutionBackendError> {
        match &self.service {
            Service::Local(sessions) => sessions.stop_jobs(by, grace),
            Service::Runner(sessions, runtime) => wait(runtime, sessions.stop_jobs(by, grace))?,
        }
    }

    /// See [`JoinedProcessSessions::stop_foreground`]. A runner port holds
    /// no foreground session of this process.
    ///
    /// # Errors
    /// Returns the service's stop error.
    pub fn stop_foreground(&self) -> Result<(), ExecutionBackendError> {
        match &self.service {
            Service::Local(sessions) => sessions.stop_foreground(),
            Service::Runner(..) => Ok(()),
        }
    }

    /// See [`JoinedProcessSessions::disown_leftovers`].
    ///
    /// # Errors
    /// Returns the service's disown error.
    pub fn disown_leftovers(&self) -> Result<(), ExecutionBackendError> {
        match &self.service {
            Service::Local(sessions) => sessions.disown_leftovers(),
            Service::Runner(sessions, runtime) => wait(runtime, sessions.disown_leftovers())?,
        }
    }
}

/// Waits for `effect` on a thread of its own, so a caller on any thread,
/// async worker or not, gets the runner's answer.
fn wait<T: Send>(
    runtime: &tokio::runtime::Handle,
    effect: impl Future<Output = T> + Send,
) -> Result<T, ExecutionBackendError> {
    std::thread::scope(|scope| scope.spawn(|| runtime.block_on(effect)).join())
        .map_err(|_| crate::environment::runner::waiter_failed())
}
