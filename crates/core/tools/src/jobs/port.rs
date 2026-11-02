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
/// registry finds it again by that owner's session and branch. Today every
/// port is the in-process [`JoinedProcessSessions`] of the host backend, and
/// each operation has that service's exact semantics.
///
/// A remote service becomes a second variant of this handle, not a trait a
/// caller implements. Its read accessors ([`Self::jobs`], the counts and
/// flags, the terminal generation and its change watch,
/// [`Self::terminal_jobs_since`], [`Self::session_for_number`] and
/// [`Self::leftover`]) are expected to be served from a host-side mirror the
/// runner's job events keep current; [`Self::observe_jobs`] is fed by those
/// events. The effects ([`Self::stop_with_grace`], [`Self::stop_jobs`],
/// [`Self::stop_foreground`] and [`Self::disown_leftovers`]) are forwarded to
/// the runner and become async; [`Self::bind_host_capacity`] and
/// [`Self::bind_job_outputs`] are forwarded once at attach.
#[derive(Clone, Debug)]
pub struct JoinedSessionsPort {
    local: JoinedProcessSessions,
}

impl JoinedSessionsPort {
    /// The port of an in-process service.
    #[must_use]
    pub const fn local(sessions: JoinedProcessSessions) -> Self {
        Self { local: sessions }
    }

    /// The in-process service behind this port, when it is one. An
    /// environment adopts only a service of its own process.
    #[must_use]
    pub const fn as_local(&self) -> Option<&JoinedProcessSessions> {
        Some(&self.local)
    }

    /// See [`JoinedProcessSessions::bind_host_capacity`].
    pub fn bind_host_capacity(&self, capacity: JoinedProcessHostCapacity) {
        self.local.bind_host_capacity(capacity);
    }

    /// See [`JoinedProcessSessions::bind_job_outputs`].
    pub fn bind_job_outputs(
        &self,
        root: JobOutputRoot,
        session: plakous_model::SessionId,
        branch: plakous_model::BranchId,
    ) {
        self.local.bind_job_outputs(root, session, branch);
    }

    /// See [`JoinedProcessSessions::observe_jobs`].
    pub fn observe_jobs(
        &self,
        observer: impl Fn(JobView) -> bool + Send + Sync + 'static,
        has_room: impl Fn() -> bool + Send + Sync + 'static,
    ) {
        self.local.observe_jobs(observer, has_room);
    }

    /// See [`JoinedProcessSessions::jobs`].
    #[must_use]
    pub fn jobs(&self) -> Vec<JobView> {
        self.local.jobs()
    }

    /// See [`JoinedProcessSessions::running_job_count`].
    #[must_use]
    pub fn running_job_count(&self) -> usize {
        self.local.running_job_count()
    }

    /// See [`JoinedProcessSessions::has_jobs`].
    #[must_use]
    pub fn has_jobs(&self) -> bool {
        self.local.has_jobs()
    }

    /// See [`JoinedProcessSessions::has_unreported_terminal_job`].
    #[must_use]
    pub fn has_unreported_terminal_job(&self) -> bool {
        self.local.has_unreported_terminal_job()
    }

    /// See [`JoinedProcessSessions::job_admission_closed`].
    #[must_use]
    pub fn job_admission_closed(&self) -> bool {
        self.local.job_admission_closed()
    }

    /// See [`JoinedProcessSessions::job_terminal_generation`].
    #[must_use]
    pub fn job_terminal_generation(&self) -> u64 {
        self.local.job_terminal_generation()
    }

    /// See [`JoinedProcessSessions::job_terminal_changes`].
    #[must_use]
    pub fn job_terminal_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.local.job_terminal_changes()
    }

    /// See [`JoinedProcessSessions::terminal_jobs_since`].
    #[must_use]
    pub fn terminal_jobs_since(&self, generation: u64) -> Vec<String> {
        self.local.terminal_jobs_since(generation)
    }

    /// See [`JoinedProcessSessions::session_for_number`].
    #[must_use]
    pub fn session_for_number(&self, number: u32) -> Option<JoinedProcessSessionId> {
        self.local.session_for_number(number)
    }

    /// See [`JoinedProcessSessions::leftover`].
    #[must_use]
    pub fn leftover(&self, id: JoinedProcessSessionId) -> Option<(Option<i32>, usize)> {
        self.local.leftover(id)
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
        self.local.stop_with_grace(id, by, grace)
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
        self.local.stop_jobs(by, grace)
    }

    /// See [`JoinedProcessSessions::stop_foreground`].
    ///
    /// # Errors
    /// Returns the service's stop error.
    pub fn stop_foreground(&self) -> Result<(), ExecutionBackendError> {
        self.local.stop_foreground()
    }

    /// See [`JoinedProcessSessions::disown_leftovers`].
    ///
    /// # Errors
    /// Returns the service's disown error.
    pub fn disown_leftovers(&self) -> Result<(), ExecutionBackendError> {
        self.local.disown_leftovers()
    }
}
