//! Neutral session-owned process observations and host ownership seam.

use crate::{ExecutionBackendError, JoinedProcessSessions};
use plakous_model::{AgentId, BranchId, SessionId, Timestamp};
use std::time::Duration;

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
    /// Owning agent's non-secret identifier.
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

/// Stable host key for one runtime's process service.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JobOwner {
    /// Runtime session.
    pub session_id: SessionId,
    /// Runtime branch.
    pub branch_id: BranchId,
    /// Agent attribution.
    pub agent_id: AgentId,
}

/// Optional host-owned registry. Without it the agent privately owns its service.
///
/// A host may transfer the same service on a same-branch reopen. It must stop
/// foreground sessions before returning transferred authority, and must not
/// transfer services across branches.
pub trait JobHost: Send + Sync {
    /// Reads only the service owned by this exact runtime session and branch.
    fn service(&self, session: SessionId, branch: BranchId) -> Option<JoinedProcessSessions>;
    /// Observes terminal process snapshots immediately before committed notice
    /// consumption removes them. Observation grants no process authority.
    fn observe_terminal(&self, _owner: JobOwner, _jobs: &[JobView]) {}

    /// Installs or transfers the exact process service for this owner.
    ///
    /// # Errors
    /// Returns a typed error when host ownership cannot be established.
    fn attach(
        &self,
        owner: JobOwner,
        fresh: JoinedProcessSessions,
    ) -> Result<JoinedProcessSessions, ExecutionBackendError>;
}
