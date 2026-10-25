//! A worker refused before any cell source can be dispatched.

use std::fmt;

use crate::WorkerIdentity;

/// A definite worker startup failure, not an unknown cell outcome.
#[derive(Debug)]
pub struct CellStartupError {
    worker: &'static str,
    cause: String,
    timed_out: bool,
}

impl CellStartupError {
    pub(super) fn new(expected: WorkerIdentity, cause: String) -> Self {
        Self {
            worker: expected.binary,
            cause,
            timed_out: false,
        }
    }

    pub(super) fn timeout(expected: WorkerIdentity, cause: String) -> Self {
        Self {
            timed_out: true,
            ..Self::new(expected, cause)
        }
    }

    pub(super) fn unreaped(mut self) -> Self {
        self.cause
            .push_str("; cell worker remained unreaped after kill deadline");
        self
    }
}

impl fmt::Display for CellStartupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.timed_out {
            return write!(
                formatter,
                "Code Mode worker did not answer within 10 s ({}); retry if the host is overloaded, otherwise reinstall the matching {}",
                self.cause, self.worker
            );
        }
        write!(
            formatter,
            "Code Mode worker is incompatible or did not start: {}; reinstall the matching {}",
            self.cause, self.worker
        )
    }
}

impl std::error::Error for CellStartupError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MONTY_WORKER, QUICKJS_WORKER};

    #[test]
    fn python_startup_failures_name_the_python_worker() {
        let message = CellStartupError::new(MONTY_WORKER, "closed before hello".into()).to_string();
        assert_eq!(
            message,
            "Code Mode worker is incompatible or did not start: closed before hello; reinstall the matching plakous-py-host"
        );
    }

    #[test]
    fn timeout_guidance_distinguishes_host_load_from_incompatibility() {
        for worker in [MONTY_WORKER, QUICKJS_WORKER] {
            let message = CellStartupError::timeout(worker, "hello timed out".into()).to_string();
            assert_eq!(
                message,
                format!(
                    "Code Mode worker did not answer within 10 s (hello timed out); retry if the host is overloaded, otherwise reinstall the matching {}",
                    worker.binary
                )
            );
            assert!(!message.contains("incompatible"));
        }
    }
}
