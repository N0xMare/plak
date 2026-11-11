//! Line-owned process observations, retained local services and owner-scoped runner ports.

use crate::ExecutionBackendError;
use plakous_model::{BranchId, LineId, SessionId, Timestamp};
use std::time::Duration;

mod port;
pub use port::JoinedSessionsPort;

/// Shared admission bound for at most 128 running processes per host.
pub type JobHostCapacity = crate::JoinedProcessHostCapacity;

/// Construction-bound process budget and hard runtime cap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JobConfig {
    foreground_seconds: u64,
    max_runtime_hours: u64,
}

impl Default for JobConfig {
    fn default() -> Self {
        Self {
            foreground_seconds: 30,
            max_runtime_hours: 10,
        }
    }
}

impl JobConfig {
    /// Validates the closed product-independent construction bounds.
    ///
    /// # Errors
    /// Rejects budgets outside 1..=600 seconds or caps outside 1..=24 hours.
    pub fn new(
        foreground_seconds: u64,
        max_runtime_hours: u64,
    ) -> Result<Self, crate::ExecutionEnvironmentError> {
        if !(1..=600).contains(&foreground_seconds) || !(1..=24).contains(&max_runtime_hours) {
            return Err(crate::ExecutionEnvironmentError::new(
                crate::ExecutionEnvironmentErrorKind::InvalidLimit,
                "job construction bounds are invalid",
            ));
        }
        Ok(Self {
            foreground_seconds,
            max_runtime_hours,
        })
    }
    /// Default foreground observation budget.
    #[must_use]
    pub const fn foreground_seconds(self) -> u64 {
        self.foreground_seconds
    }
    /// Hard process runtime cap in hours.
    #[must_use]
    pub const fn max_runtime_hours(self) -> u64 {
        self.max_runtime_hours
    }
    /// Applies both values to the exact environment descriptor limits.
    ///
    /// # Errors
    /// Rejects a cap that contradicts another environment execution limit.
    pub fn limits(
        self,
        limits: crate::ExecutionEnvironmentLimits,
    ) -> Result<crate::ExecutionEnvironmentLimits, crate::ExecutionEnvironmentError> {
        limits
            .with_session_duration(Duration::from_secs(self.max_runtime_hours * 3600))?
            .with_foreground_seconds(self.foreground_seconds)
    }
}

/// Attribution for an explicitly stopped process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobStopReason {
    /// Model stop tool.
    Model,
    /// Human stop action.
    User,
    /// Session release.
    SessionExit,
    /// Owning attempt ended.
    AttemptEnded,
    /// Construction-bound runtime cap.
    HardCap,
}

/// Process-local terminal facts; never persisted as process authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobStatus {
    /// The owned process is running.
    Running,
    /// Natural exit and its recorded status.
    Exited(Option<i32>),
    /// Explicit stop with its attribution.
    Stopped(JobStopReason),
    /// Ownership was explicitly relinquished; this does not prove process exit.
    Released,
    /// No exact terminal outcome is available.
    OutcomeUnknown,
}

/// Snapshot of a live or terminal-but-unconsumed process job.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobView {
    /// Number shared by `p<N>` and the vendor handle.
    pub number: u32,
    /// Bounded displayed command.
    pub command: String,
    /// UTC process start.
    pub started_at: Timestamp,
    /// Display attribution of the owner: the driver instance for a live
    /// process snapshot, the owning agent line for a view read from a
    /// durable job row. Never authority.
    pub owner: String,
    /// Time since process start (fixed after exit).
    pub elapsed: Duration,
    /// Current or snapshotted terminal state.
    pub status: JobStatus,
    /// Retained output tail, bounded by 40 lines and 4 KiB.
    pub output: String,
    /// Allocation-local identity, never a PID.
    pub session_id: [u8; 16],
    /// Allocation-local owner generation.
    pub owner_generation: u64,
}

/// Owner of one line's process service: the line head that holds it.
///
/// A line's head moves only forward, so `(generation, fence)` orders every
/// owner of one line: a rebind advances `generation` and hands the service to
/// the new head; a same-branch reopen keeps `generation` and takes a higher
/// `fence`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JobOwner {
    /// The agent line that owns the processes.
    pub line: LineId,
    /// The line generation of the owning head branch.
    pub generation: u64,
    /// The head branch's runtime session.
    pub session_id: SessionId,
    /// The head branch.
    pub branch_id: BranchId,
    /// The owning lease fence (0 for an ephemeral agent).
    pub fence: u64,
}

impl JobOwner {
    /// Whether `self` may take a service `previous` holds: the same line, and
    /// either a later generation (a line hand-off) or the same head branch
    /// under an equal or later fence (a same-branch reopen).
    #[must_use]
    pub fn succeeds(&self, previous: &Self) -> bool {
        self.line == previous.line
            && (self.generation > previous.generation
                || (self.generation == previous.generation
                    && self.session_id == previous.session_id
                    && self.branch_id == previous.branch_id
                    && self.fence >= previous.fence))
    }
}

/// Optional host-owned registry, keyed by agent line. Without it the agent
/// privately owns its service, and its jobs stop when it drops.
///
/// A host holds each line's service as a [`JoinedSessionsPort`] and admits
/// only a [`JobOwner::succeeds`] owner: the line's next head or the same head
/// on a reopen. Two local ports retain the live in-process service; if either
/// port is a runner port, the fresh port replaces it so operations use the
/// current owner's exact runner scope and backend kind. Runner jobs remain
/// on the runner. It must stop foreground sessions before returning authority,
/// and must never hand a service to another line or to an older owner.
pub trait JobHost: Send + Sync {
    /// Reads only the service owned by this exact line.
    fn service(&self, line: LineId) -> Option<JoinedSessionsPort>;
    /// Reads the service whose current owner is this exact head branch, for
    /// a host that knows a branch rather than its line. The default finds
    /// none.
    fn service_at(&self, _session: SessionId, _branch: BranchId) -> Option<JoinedSessionsPort> {
        None
    }
    /// Observes terminal process snapshots immediately before committed notice
    /// consumption removes them. Observation grants no process authority.
    fn observe_terminal(&self, _owner: JobOwner, _jobs: &[JobView]) {}

    /// Installs or transfers the exact process service for this owner.
    ///
    /// # Errors
    /// Returns a typed error when host ownership cannot be established,
    /// including an attach by an owner that does not succeed the holder.
    fn attach(
        &self,
        owner: JobOwner,
        fresh: JoinedSessionsPort,
    ) -> Result<JoinedSessionsPort, ExecutionBackendError>;
}

/// An in-memory [`JobHost`] keyed by agent line.
///
/// Installing one on every agent of a line gives an embedder process
/// continuity across a rebind: local-to-local attaches retain the source's
/// live service, running jobs included. If either port is a runner port, the
/// target takes its fresh port instead. Registry entries live as long as this
/// value; replaced ports drop when their remaining runtime handles drop.
#[derive(Default)]
pub struct LocalJobHost {
    services: std::sync::Mutex<std::collections::HashMap<LineId, (JobOwner, JoinedSessionsPort)>>,
    capacity: JobHostCapacity,
}

impl LocalJobHost {
    /// Creates an empty registry with the shared 128-running host bound.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl std::fmt::Debug for LocalJobHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let lines = self.services.lock().map_or(0, |services| services.len());
        formatter
            .debug_struct("LocalJobHost")
            .field("lines", &lines)
            .finish_non_exhaustive()
    }
}

impl JobHost for LocalJobHost {
    fn service(&self, line: LineId) -> Option<JoinedSessionsPort> {
        self.services
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&line)
            .map(|(_, service)| service.clone())
    }

    fn service_at(&self, session: SessionId, branch: BranchId) -> Option<JoinedSessionsPort> {
        self.services
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .find(|(owner, _)| owner.session_id == session && owner.branch_id == branch)
            .map(|(_, service)| service.clone())
    }

    fn attach(
        &self,
        owner: JobOwner,
        fresh: JoinedSessionsPort,
    ) -> Result<JoinedSessionsPort, ExecutionBackendError> {
        let mut services = self
            .services
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        services.retain(|_, (_, service)| !service.job_admission_closed() || service.has_jobs());
        if services
            .get(&owner.line)
            .is_some_and(|(holder, _)| !owner.succeeds(holder))
        {
            return Err(ExecutionBackendError::stale_job_owner());
        }
        let service = if let Some((holder, service)) = services.get_mut(&owner.line)
            && !fresh.is_runner()
            && !service.is_runner()
        {
            *holder = owner;
            service.clone()
        } else {
            fresh.bind_host_capacity(self.capacity.clone());
            services.insert(owner.line, (owner, fresh.clone()));
            fresh
        };
        drop(services);
        // Foreground calls belong to the previous owner's turn.
        service.stop_foreground()?;
        Ok(service)
    }
}

#[cfg(test)]
mod tests {
    use super::{BranchId, JobOwner, LineId, SessionId};

    fn owner(line: &str, generation: u64, branch: &str, fence: u64) -> JobOwner {
        let line: BranchId = line.parse().unwrap_or_else(|_| unreachable!());
        JobOwner {
            line: LineId::started_by(line),
            generation,
            session_id: "00000000-0000-0000-0000-000000000001"
                .parse::<SessionId>()
                .unwrap_or_else(|_| unreachable!()),
            branch_id: branch.parse().unwrap_or_else(|_| unreachable!()),
            fence,
        }
    }

    /// Only the same line's next generation, or the same head under an equal
    /// or higher fence, succeeds a holder; never another line.
    #[test]
    fn only_the_same_lines_later_owner_succeeds() {
        const LINE: &str = "00000000-0000-0000-0000-00000000000a";
        const OTHER: &str = "00000000-0000-0000-0000-00000000000b";
        const HEAD: &str = "00000000-0000-0000-0000-00000000000c";
        const NEXT: &str = "00000000-0000-0000-0000-00000000000d";
        let holder = owner(LINE, 2, HEAD, 3);
        assert!(owner(LINE, 3, NEXT, 1).succeeds(&holder));
        assert!(owner(LINE, 2, HEAD, 3).succeeds(&holder));
        assert!(owner(LINE, 2, HEAD, 4).succeeds(&holder));
        assert!(!owner(LINE, 2, HEAD, 2).succeeds(&holder));
        assert!(!owner(LINE, 1, HEAD, 9).succeeds(&holder));
        assert!(!owner(LINE, 2, NEXT, 9).succeeds(&holder));
        assert!(!owner(OTHER, 9, HEAD, 9).succeeds(&holder));
    }
}
