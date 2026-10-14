//! Non-droppable job transitions, captured before canonical consumption.

use plakous::tools::{JobStatus, JobStopReason, JobView};
use plakous_host_protocol::SessionObservation;
use std::collections::HashMap;

#[derive(Default)]
pub(crate) struct JobEvents {
    reported: HashMap<[u8; 16], u8>,
    generation: Option<u64>,
}

pub(crate) fn transition<P: plakous_host_protocol::ProtocolExtension>(
    phase: u8,
    job: &JobView,
) -> SessionObservation<P> {
    let job_id = format!("p{}", job.number);
    if phase == 0 {
        return SessionObservation::JobStarted {
            job_id,
            owner: job.owner.clone(),
            command: job.command.clone(),
        };
    }
    if phase == 1 {
        return SessionObservation::JobPromoted {
            job_id,
            owner: job.owner.clone(),
        };
    }
    let (status, exit_code, stop_reason) = match job.status {
        JobStatus::Running => unreachable!(),
        JobStatus::Exited(code) => ("exited", code, None),
        JobStatus::OutcomeUnknown => ("outcome_unknown", None, None),
        JobStatus::Stopped(reason) => (
            "stopped",
            None,
            Some(
                match reason {
                    JobStopReason::Model => "model",
                    JobStopReason::User => "user",
                    JobStopReason::SessionExit => "session_exit",
                    JobStopReason::AttemptEnded => "attempt_ended",
                    JobStopReason::HardCap => "hard_cap",
                }
                .into(),
            ),
        ),
    };
    SessionObservation::JobTerminal {
        job_id,
        owner: job.owner.clone(),
        status: status.into(),
        exit_code,
        stop_reason,
        elapsed_ms: u64::try_from(job.elapsed.as_millis()).unwrap_or(u64::MAX),
        output: job.output.clone(),
    }
}

/// The transcript row of a job's terminal transition.
fn process_row(job: &JobView) -> Option<crate::sessions::TranscriptRow> {
    use plakous_host_protocol::views::transcript::{ProcessNotice, ProcessOutcome};
    let outcome = match job.status {
        JobStatus::Exited(code) => ProcessOutcome::Exited { code },
        JobStatus::Stopped(_) => ProcessOutcome::Stopped,
        JobStatus::Running | JobStatus::OutcomeUnknown => return None,
    };
    Some(crate::sessions::TranscriptRow::Process(ProcessNotice {
        job_id: format!("p{}", job.number),
        outcome,
        duration_ms: Some(u64::try_from(job.elapsed.as_millis()).unwrap_or(u64::MAX)),
        subject: process_subject(&job.command),
    }))
}

/// The command a process row names: redacted and bounded the way a tool
/// subject is (spec 04 §5), one line.
fn process_subject(command: &str) -> Option<String> {
    let command = command.trim();
    (!command.is_empty()).then(|| super::events::live_tool_subject(command))
}

impl JobEvents {
    fn publish<P: plakous_host_protocol::ProtocolExtension>(
        &mut self,
        phase: u8,
        job: &JobView,
        send: &mut impl FnMut(SessionObservation<P>) -> bool,
    ) -> bool {
        if self
            .reported
            .get(&job.session_id)
            .is_some_and(|prior| *prior >= phase)
        {
            return true;
        }
        if !send(transition(phase, job)) {
            return false;
        }
        self.reported.insert(job.session_id, phase);
        true
    }

    /// Publish pending transitions. Returns process rows for terminal
    /// transitions the attached root owns, including a job on an earlier fork
    /// or rebind of that root, for the caller to persist. Observations are
    /// published for every owner. Attach-time snapshot reconciliation
    /// republishes observations only, so a child terminal there persists no row.
    pub(crate) fn emit<P: plakous_host_protocol::ProtocolExtension>(
        &mut self,
        registry: &crate::jobs::JobRegistry,
        sender: &super::Sender<P>,
    ) -> Vec<crate::sessions::TranscriptRow> {
        let mut send = |event| sender.send(event);
        let mut rows = Vec::new();
        let root_branches = registry.root_branches();
        registry.publish_pending(|phase, key, job| {
            let published = self.publish(phase, job, &mut send);
            if published && phase == 2 && root_branches.contains(&key) {
                rows.extend(process_row(job));
            }
            published
        });
        let generation = registry.generation();
        if self.generation.is_some() {
            registry.retain_reported(&mut self.reported);
            return rows;
        }
        // Snapshot reconciliation is for attachment/reopen only. Live lifecycle
        // facts are retained independently of the row and retried above.
        for job in registry.views() {
            for phase in 0..=if job.status == JobStatus::Running {
                1
            } else {
                2
            } {
                if !self.publish(phase, &job, &mut send) {
                    return rows;
                }
            }
        }
        self.generation = Some(generation);
        registry.retain_reported(&mut self.reported);
        rows
    }

    #[cfg(test)]
    fn observe<P: plakous_host_protocol::ProtocolExtension>(
        &mut self,
        jobs: &[JobView],
    ) -> Vec<SessionObservation<P>> {
        let mut events = Vec::new();
        for job in jobs {
            for phase in 0..=if job.status == JobStatus::Running {
                1
            } else {
                2
            } {
                self.publish(phase, job, &mut |event| {
                    events.push(event);
                    true
                });
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plakous::{model::Timestamp, tools::JobStatus};

    #[test]
    fn job_events_report_promotion_and_terminal_once() {
        let mut events = JobEvents::default();
        let mut job = JobView {
            number: 1,
            command: "read line".into(),
            started_at: Timestamp::new(1, 0).unwrap(),
            owner: "root".into(),
            elapsed: std::time::Duration::from_millis(250),
            status: JobStatus::Running,
            output: String::new(),
            session_id: [1; 16],
            owner_generation: 1,
        };
        assert_eq!(
            events
                .observe::<plakous_host_protocol::Core>(&[job.clone()])
                .len(),
            2
        );
        assert_eq!(
            events.observe::<plakous_host_protocol::Core>(&[job.clone()]),
            [] as [SessionObservation; 0]
        );
        job.status = JobStatus::Exited(Some(0));
        assert_eq!(
            events
                .observe::<plakous_host_protocol::Core>(&[job.clone()])
                .len(),
            1
        );
        assert_eq!(
            events.observe::<plakous_host_protocol::Core>(&[job]),
            [] as [SessionObservation; 0]
        );
    }

    fn exited(command: &str) -> JobView {
        JobView {
            number: 261,
            command: command.into(),
            started_at: Timestamp::new(1, 0).unwrap(),
            owner: "root".into(),
            elapsed: std::time::Duration::from_millis(1_300_000),
            status: JobStatus::Exited(Some(0)),
            output: String::new(),
            session_id: [2; 16],
            owner_generation: 1,
        }
    }

    fn subject(command: &str) -> Option<String> {
        let Some(crate::sessions::TranscriptRow::Process(notice)) = process_row(&exited(command))
        else {
            panic!("an exited job has a process row");
        };
        notice.subject
    }

    #[test]
    fn process_row_names_its_command_redacted_on_one_bounded_line() {
        assert_eq!(subject("just check").as_deref(), Some("just check"));
        assert_eq!(
            subject("GITHUB_TOKEN=ghp_abcdefghijklmnopqrst gh pr list").as_deref(),
            Some("GITHUB_TOKEN=[redacted] gh pr list")
        );
        assert_eq!(
            subject("make\n&& make test").as_deref(),
            Some("make↵&& make test")
        );
        let long = subject(&"x".repeat(1_000)).unwrap();
        assert!(long.len() <= 240 && long.ends_with('…'), "{long}");
        assert_eq!(subject("   "), None);
    }
}
