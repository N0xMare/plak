//! The process-session service of one job owner, as a [`crate::JobHost`] holds it.

use std::time::Duration;

use crate::{
    ExecutionBackendError, JobOutputRoot, JobStopReason, JobView, JoinedProcessHostCapacity,
    JoinedProcessSessionId, JoinedProcessSessions, JoinedProcessStatus,
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
/// A runner port (unsupported, [`Self::runner`]) holds the service of an
/// environment on another runner: this process's job bookkeeping over one
/// line's sessions there, whose process operations go to the runner and wait
/// for its answer on a thread of their own, and whose job reads and terminal
/// notices follow the runner's job events. The runner keeps its own
/// capacity, so [`Self::bind_host_capacity`] does nothing for it.
#[derive(Clone, Debug)]
pub struct JoinedSessionsPort {
    service: Service,
}

#[derive(Clone, Debug)]
enum Service {
    Local(JoinedProcessSessions),
    Runner(JoinedProcessSessions),
}

impl JoinedSessionsPort {
    /// The port of an in-process service.
    #[must_use]
    pub const fn local(sessions: JoinedProcessSessions) -> Self {
        Self {
            service: Service::Local(sessions),
        }
    }

    /// The port of an environment's service on another runner
    /// (`internal::runner::adopt_runner_process_sessions`). Unsupported:
    /// only a test route selects a runner.
    #[doc(hidden)]
    #[must_use]
    pub const fn runner(sessions: JoinedProcessSessions) -> Self {
        Self {
            service: Service::Runner(sessions),
        }
    }

    /// Whether this port's runner scope must be rebuilt for a new owner.
    #[doc(hidden)]
    #[must_use]
    pub const fn is_runner(&self) -> bool {
        matches!(self.service, Service::Runner(_))
    }

    const fn sessions(&self) -> &JoinedProcessSessions {
        match &self.service {
            Service::Local(sessions) | Service::Runner(sessions) => sessions,
        }
    }

    /// The service behind this port. An environment adopts only a service
    /// of its own process, which a runner port's service also is.
    #[must_use]
    pub const fn as_local(&self) -> Option<&JoinedProcessSessions> {
        Some(self.sessions())
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
        self.sessions().bind_job_outputs(root, session, branch);
    }

    /// See [`JoinedProcessSessions::observe_jobs`].
    pub fn observe_jobs(
        &self,
        observer: impl Fn(JobView) -> bool + Send + Sync + 'static,
        has_room: impl Fn() -> bool + Send + Sync + 'static,
    ) {
        self.sessions().observe_jobs(observer, has_room);
    }

    /// See [`JoinedProcessSessions::jobs`].
    #[must_use]
    pub fn jobs(&self) -> Vec<JobView> {
        self.sessions().jobs()
    }

    /// See [`JoinedProcessSessions::running_job_count`].
    #[must_use]
    pub fn running_job_count(&self) -> usize {
        self.sessions().running_job_count()
    }

    /// See [`JoinedProcessSessions::has_jobs`].
    #[must_use]
    pub fn has_jobs(&self) -> bool {
        self.sessions().has_jobs()
    }

    /// See [`JoinedProcessSessions::has_unreported_terminal_job`].
    #[must_use]
    pub fn has_unreported_terminal_job(&self) -> bool {
        self.sessions().has_unreported_terminal_job()
    }

    /// See [`JoinedProcessSessions::job_admission_closed`].
    #[must_use]
    pub fn job_admission_closed(&self) -> bool {
        self.sessions().job_admission_closed()
    }

    /// See [`JoinedProcessSessions::job_terminal_generation`].
    #[must_use]
    pub fn job_terminal_generation(&self) -> u64 {
        self.sessions().job_terminal_generation()
    }

    /// See [`JoinedProcessSessions::job_terminal_changes`].
    #[must_use]
    pub fn job_terminal_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.sessions().job_terminal_changes()
    }

    /// See [`JoinedProcessSessions::terminal_jobs_since`].
    #[must_use]
    pub fn terminal_jobs_since(&self, generation: u64) -> Vec<String> {
        self.sessions().terminal_jobs_since(generation)
    }

    /// See [`JoinedProcessSessions::session_for_number`].
    #[must_use]
    pub fn session_for_number(&self, number: u32) -> Option<JoinedProcessSessionId> {
        self.sessions().session_for_number(number)
    }

    /// See [`JoinedProcessSessions::leftover`].
    #[must_use]
    pub fn leftover(&self, id: JoinedProcessSessionId) -> Option<(Option<i32>, usize)> {
        self.sessions().leftover(id)
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
        self.sessions().stop_with_grace(id, by, grace)
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
        self.sessions().stop_jobs(by, grace)
    }

    /// See [`JoinedProcessSessions::stop_foreground`].
    ///
    /// # Errors
    /// Returns the service's stop error.
    pub fn stop_foreground(&self) -> Result<(), ExecutionBackendError> {
        self.sessions().stop_foreground()
    }

    /// See [`JoinedProcessSessions::disown_leftovers`].
    ///
    /// # Errors
    /// Returns the service's disown error.
    pub fn disown_leftovers(&self) -> Result<(), ExecutionBackendError> {
        self.sessions().disown_leftovers()
    }

    /// Seeds the durable per-session job number counter without ever moving
    /// it back (`internal::seed_job_counter`).
    #[doc(hidden)]
    pub fn seed_job_counter(&self, next_id: u32) {
        self.sessions().seed_job_counter(next_id);
    }

    /// Hydrates durable job rows as read-only terminal snapshots; never
    /// reconstructs a live process (`internal::restore_job_terminals`).
    #[doc(hidden)]
    pub fn restore_job_terminals(&self, views: &[JobView]) {
        self.sessions().restore_job_terminals(views);
    }

    /// Drops terminal snapshots whose observation has committed
    /// (`internal::consume_jobs`).
    #[doc(hidden)]
    pub fn consume_jobs(&self, numbers: &[u32]) {
        self.sessions().consume_jobs(numbers);
    }

    /// Reads a runner service's jobs from its runner before a restore
    /// matches them against durable rows; nothing for a local service.
    ///
    /// # Errors
    ///
    /// The runner did not answer; the restore must not run without it.
    #[doc(hidden)]
    pub fn refresh_jobs(&self) -> Result<(), ExecutionBackendError> {
        self.sessions().refresh_runner_jobs()
    }

    /// Lets a runner service's job events report once its restore is done;
    /// nothing for a local service.
    #[doc(hidden)]
    pub fn settle_jobs(&self) {
        self.sessions().settle_runner_jobs();
    }

    /// Installs this port's service as `environment`'s process service.
    #[doc(hidden)]
    pub fn adopt_into(&self, environment: &crate::ExecutionEnvironment) {
        environment.adopt_joined_process_sessions(self.sessions());
    }
}
