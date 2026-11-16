//! Join allocation identities with observations, never with apparent PID liveness.
use super::{JobCut, Jobs, Status, Stopper};
use plakous::tools::{JobStatus, JobStopReason, JobView};
use std::collections::BTreeSet;

impl Jobs {
    pub(in crate::carry::host_move) fn needs_runner(&self) -> bool {
        self.rows.iter().any(|row| row.terminal.is_none())
    }
    pub(crate) fn verify_inventory(&self, observed: &[JobView], cut: JobCut) -> Result<(), String> {
        let mut seen = BTreeSet::new();
        for job in observed {
            let Some(row) = self.rows.iter().find(|row| row.number == job.number) else {
                return Err("runner reports an uncarried job allocation".into());
            };
            if !seen.insert(job.number)
                || job.session_id != row.session
                || job.owner_generation != row.owner_generation
                || job.status == JobStatus::OutcomeUnknown
                || matches!(cut, JobCut::Settled) && job.status == JobStatus::Running
                || row
                    .terminal
                    .as_ref()
                    .is_some_and(|terminal| !terminal.status.matches(job.status))
            {
                return Err("runner job allocation or terminal evidence changed".into());
            }
        }
        if self
            .rows
            .iter()
            .any(|row| row.terminal.is_none() && !seen.contains(&row.number))
        {
            return Err("a promoted allocation has no fresh runner observation".into());
        }
        Ok(())
    }
}

impl Status {
    fn matches(&self, actual: JobStatus) -> bool {
        match self {
            Self::Exited { code } => actual == JobStatus::Exited(*code),
            Self::Released {} => actual == JobStatus::Released,
            Self::Stopped { by } => {
                actual
                    == JobStatus::Stopped(match by {
                        Stopper::Model => JobStopReason::Model,
                        Stopper::User => JobStopReason::User,
                        Stopper::SessionExit => JobStopReason::SessionExit,
                        Stopper::AttemptEnded => JobStopReason::AttemptEnded,
                        Stopper::HardCap => JobStopReason::HardCap,
                    })
            }
        }
    }
}
