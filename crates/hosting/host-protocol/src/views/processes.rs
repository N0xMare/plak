//! Observational background-process rows; identifiers are not process authority.

/// A live or recently finished background process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessView {
    /// Host-local opaque identity, stable across same-branch reopen.
    pub id: String,
    /// One-line command.
    pub command: String,
    /// Owning agent identifier (`root` or a child id).
    pub owner: String,
    /// UTC start in Unix seconds.
    pub started_at: u64,
    /// Elapsed milliseconds, fixed after completion.
    pub elapsed_ms: u64,
    /// Current process state.
    pub status: ProcessStatus,
    /// Bounded terminal-text output tail.
    pub output: String,
}

/// Process-local outcome, never durable runtime authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessStatus {
    /// Still running.
    Running,
    /// A user stop is underway: SIGTERM sent, SIGKILL after its grace.
    Stopping,
    /// Natural exit; no code when the platform did not provide one.
    Exited(Option<i32>),
    /// Explicitly stopped.
    Stopped,
    /// Exact outcome unavailable.
    OutcomeUnknown,
}

impl ProcessStatus {
    /// Whether the process may still be alive: running or being stopped.
    #[must_use]
    pub const fn is_live(self) -> bool {
        matches!(self, Self::Running | Self::Stopping)
    }
}
