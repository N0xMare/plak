//! Lifecycle barriers for the paired restart fixture.

use super::{JobStatus, JoinedProcessSessionId, JoinedProcessSessions, RunnerSessions};

pub(super) fn acknowledged(numbers: &[u32]) {
    for &number in numbers {
        mark("ack", number);
    }
}

pub(super) fn terminal_projection(sessions: &RunnerSessions, id: JoinedProcessSessionId) {
    if sessions
        .cached_job_status(id)
        .is_some_and(|status| status != JobStatus::Running)
    {
        mark("terminal-projection", JoinedProcessSessions::job_number(id));
    }
}

fn mark(event: &str, number: u32) {
    if let Some(directory) = std::env::var_os("PLAK_TEST_RUNNER_JOB_PROBE") {
        let _ = std::fs::write(
            std::path::Path::new(&directory).join(format!("{event}-{number}")),
            [],
        );
    }
}
