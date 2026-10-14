//! Session-owned process metadata, independent of recovery obligations.

use super::{ProcessSessionId, ProcessSessionOwnerGeneration, RuntimeManifestError};
use plakous_model::Timestamp;
use plakous_tools::{JobStatus, JobStopReason, JobView};
use std::time::Duration;

/// Maximum unconsumed process notices in one checkpoint.
pub(crate) const MAX_JOB_ROWS: usize = 128;

/// Durable terminal classification; live authority never comes from a checkpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobTerminalStatus {
    /// Natural process exit.
    Exited,
    /// Explicit stop or construction-bound runtime cap.
    Stopped,
}

/// Bounded exact process outcome carried across owner release and reopen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobTerminal {
    /// Natural exit versus attributed stop.
    pub status: JobTerminalStatus,
    /// Natural exit code, when available.
    pub exit_code: Option<i32>,
    /// Attribution for a stopped process.
    pub stopper: Option<JobStopReason>,
    /// Fixed elapsed runtime.
    pub elapsed: Duration,
    /// Output tail, at most 4 KiB.
    pub tail: String,
}

impl JobTerminal {
    pub(super) fn from_view(view: &JobView) -> Option<Self> {
        let (status, exit_code, stopper) = match view.status {
            JobStatus::Exited(code) => (JobTerminalStatus::Exited, code, None),
            JobStatus::Stopped(by) => (JobTerminalStatus::Stopped, None, Some(by)),
            JobStatus::Running | JobStatus::OutcomeUnknown => return None,
        };
        Some(Self {
            status,
            exit_code,
            stopper,
            elapsed: view.elapsed,
            tail: plakous_tools::internal::job_output_tail(&view.output),
        })
    }

    fn validate(&self) -> Result<(), RuntimeManifestError> {
        if self.tail.len() > 4096
            || self.tail.contains('\0')
            || (self.status == JobTerminalStatus::Exited && self.stopper.is_some())
            || (self.status == JobTerminalStatus::Stopped
                && (self.stopper.is_none() || self.exit_code.is_some()))
        {
            return Err(RuntimeManifestError::InvalidText {
                field: "job terminal",
            });
        }
        Ok(())
    }
}

/// Durable metadata for one promoted process, including exact terminal evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobRow {
    /// The vendor handle and the number in `p<N>`.
    pub number: u32,
    /// Bounded displayed command, not executable authority.
    pub command: String,
    /// UTC start time.
    pub started_at: Timestamp,
    /// Non-secret owning agent identity.
    pub owner: String,
    /// Allocation-local process identity; never an OS PID.
    pub session_id: ProcessSessionId,
    /// Allocation-local process owner generation.
    pub owner_generation: ProcessSessionOwnerGeneration,
    /// Exact terminal evidence, copied from the process registry on publication.
    pub terminal: Option<JobTerminal>,
}

impl JobRow {
    pub(crate) fn view(&self) -> JobView {
        JobView {
            number: self.number,
            command: self.command.clone(),
            started_at: self.started_at,
            owner: self.owner.clone(),
            elapsed: self
                .terminal
                .as_ref()
                .map_or(Duration::ZERO, |terminal| terminal.elapsed),
            status: self
                .terminal
                .as_ref()
                .map_or(JobStatus::OutcomeUnknown, |terminal| {
                    match terminal.status {
                        JobTerminalStatus::Exited => JobStatus::Exited(terminal.exit_code),
                        JobTerminalStatus::Stopped => terminal
                            .stopper
                            .map_or(JobStatus::OutcomeUnknown, JobStatus::Stopped),
                    }
                }),
            output: self
                .terminal
                .as_ref()
                .map_or_else(String::new, |terminal| terminal.tail.clone()),
            session_id: self.session_id.as_bytes(),
            owner_generation: self.owner_generation.get(),
        }
    }

    fn validate(&self) -> Result<(), RuntimeManifestError> {
        if self.number == 0
            || self.command.is_empty()
            || self.command.len() > 1024
            || self.command.contains('\0')
            || self.owner.is_empty()
            || self.owner.len() > 128
            || self.owner.chars().any(char::is_control)
        {
            return Err(RuntimeManifestError::InvalidText { field: "job row" });
        }
        if let Some(terminal) = &self.terminal {
            terminal.validate()?;
        }
        Ok(())
    }
}

/// Durable per-session number counter and unconsumed process rows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobLedger {
    next_id: u32,
    rows: Vec<JobRow>,
}

impl Default for JobLedger {
    fn default() -> Self {
        Self {
            next_id: 1,
            rows: Vec::new(),
        }
    }
}

impl JobLedger {
    /// Validates a complete bounded ledger. Rows are ordered by number.
    pub fn new(next_id: u32, rows: Vec<JobRow>) -> Result<Self, RuntimeManifestError> {
        if next_id == 0 || rows.len() > MAX_JOB_ROWS {
            return Err(RuntimeManifestError::ComponentSize);
        }
        let mut previous = 0;
        for row in &rows {
            row.validate()?;
            if row.number <= previous || row.number >= next_id {
                return Err(RuntimeManifestError::InvalidText {
                    field: "job number",
                });
            }
            previous = row.number;
        }
        Ok(Self { next_id, rows })
    }

    /// Returns the next never-before-issued process number.
    #[must_use]
    pub const fn next_id(&self) -> u32 {
        self.next_id
    }

    /// Returns live and terminal-but-unconsumed rows.
    #[must_use]
    pub fn rows(&self) -> &[JobRow] {
        &self.rows
    }

    /// Copies terminal snapshots without replacing already durable evidence.
    pub fn with_terminals(&self, views: &[JobView]) -> Result<Self, RuntimeManifestError> {
        let mut rows = self.rows.clone();
        for row in &mut rows {
            if row.terminal.is_none()
                && let Some(view) = views.iter().find(|view| {
                    view.number == row.number
                        && view.session_id == row.session_id.as_bytes()
                        && view.owner_generation == row.owner_generation.get()
                })
            {
                row.terminal = JobTerminal::from_view(view);
            }
        }
        Self::new(self.next_id, rows)
    }

    pub(crate) fn is_terminal_extension_of(&self, prior: &Self) -> bool {
        self.next_id >= prior.next_id
            && self.rows.len() == prior.rows.len()
            && self.rows.iter().zip(&prior.rows).all(|(row, before)| {
                let mut metadata = row.clone();
                metadata.terminal.clone_from(&before.terminal);
                metadata == *before
                    && (before.terminal.is_none() || row.terminal == before.terminal)
            })
    }

    /// Appends promotions and consumes terminal observations atomically.
    pub fn updated(
        &self,
        next_id: u32,
        promoted: impl IntoIterator<Item = JobRow>,
        consumed: &[u32],
    ) -> Result<Self, RuntimeManifestError> {
        let mut rows = self.rows.clone();
        for row in promoted {
            if let Some(existing) = rows.iter().find(|existing| existing.number == row.number) {
                if existing != &row {
                    return Err(RuntimeManifestError::Malformed);
                }
            } else {
                rows.push(row);
            }
        }
        rows.retain(|row| !consumed.contains(&row.number));
        rows.sort_by_key(|row| row.number);
        Self::new(self.next_id.max(next_id), rows)
    }

    /// Seeds a cleared branch without carrying pending notices.
    #[must_use]
    pub fn cleared(&self) -> Self {
        Self {
            next_id: self.next_id,
            rows: Vec::new(),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "validated fixtures preserve precise typed failure diagnostics"
)]
mod tests {
    use super::*;

    #[test]
    fn review_terminal_view_is_defensive() {
        let view = JobView {
            number: 1,
            command: "test".into(),
            started_at: Timestamp::new(1, 0).expect("timestamp"),
            owner: "agent".into(),
            elapsed: Duration::ZERO,
            status: JobStatus::Exited(Some(0)),
            output: format!("{}a\0b\u{1}", "x\n".repeat(100)),
            session_id: [1; 16],
            owner_generation: 1,
        };
        let terminal = JobTerminal::from_view(&view).expect("terminal");
        assert!(
            !terminal
                .tail
                .chars()
                .any(|ch| ch < ' ' && ch != '\n' && ch != '\t')
        );
        assert!(terminal.tail.len() <= 4096);
        assert!(terminal.tail.lines().count() <= 40);
        assert!(terminal.validate().is_ok());
        for output in [
            "x".repeat(9000),
            "\0".repeat(4096),
            "\u{1f600}".repeat(2048),
        ] {
            let terminal = JobTerminal::from_view(&JobView {
                output,
                ..view.clone()
            })
            .expect("terminal");
            assert!(terminal.tail.len() <= 4096);
            assert!(terminal.validate().is_ok());
        }
    }

    #[test]
    fn review_extension_accepts_monotonic_counter() {
        let prior = JobLedger::new(2, Vec::new()).expect("prior");
        let next = JobLedger::new(3, Vec::new()).expect("next");
        assert!(next.is_terminal_extension_of(&prior));
        assert!(!prior.is_terminal_extension_of(&next));
    }
    #[test]
    fn review_row_without_stopper_is_unknown() {
        let row = JobRow {
            number: 1,
            command: "test".into(),
            started_at: Timestamp::new(1, 0).expect("timestamp"),
            owner: "agent".into(),
            session_id: ProcessSessionId::new([1; 16]).expect("session"),
            owner_generation: ProcessSessionOwnerGeneration::new(1).expect("generation"),
            terminal: Some(JobTerminal {
                status: JobTerminalStatus::Stopped,
                exit_code: None,
                stopper: None,
                elapsed: Duration::ZERO,
                tail: String::new(),
            }),
        };
        assert_eq!(row.view().status, JobStatus::OutcomeUnknown);
    }
}
