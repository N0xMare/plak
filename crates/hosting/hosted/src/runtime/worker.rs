//! Host-owned quit disposition, available before bounded release finishes.

use crate::CliResult;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;

/// The shutdown path the single host owner actually selected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostQuitOutcome {
    /// No quit abandoned an unjoined turn by lease only.
    Clean,
    /// Quit exhausted its terminal join and selected lease-only abandonment.
    Escalated,
}

#[derive(Clone, Debug, Default)]
pub(super) struct QuitReceipt(Arc<AtomicBool>);

impl QuitReceipt {
    pub(super) fn escalate(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub(super) fn outcome(&self) -> HostQuitOutcome {
        if self.0.load(Ordering::Acquire) {
            HostQuitOutcome::Escalated
        } else {
            HostQuitOutcome::Clean
        }
    }
}

/// The host thread and its authoritative, process-local quit receipt.
/// A cooked TUI can read the disposition without waiting past its release bound.
#[derive(Debug)]
pub struct HostWorkerHandle {
    thread: thread::JoinHandle<CliResult<()>>,
    receipt: QuitReceipt,
    pub(crate) steps: Arc<super::step_hold::StepControl>,
}

impl HostWorkerHandle {
    /// Spawns a host whose ordinary completion does not escalate a quit.
    pub fn spawn(work: impl FnOnce() -> CliResult<()> + Send + 'static) -> Self {
        Self::spawn_with_receipt(move |_, _| work())
    }

    pub(super) fn spawn_with_receipt(
        work: impl FnOnce(QuitReceipt, Arc<super::step_hold::StepControl>) -> CliResult<()>
        + Send
        + 'static,
    ) -> Self {
        let receipt = QuitReceipt::default();
        let owned = receipt.clone();
        let steps = Arc::new(super::step_hold::StepControl::default());
        let lifetime = super::step_hold::StepOwner(Arc::clone(&steps));
        let control = Arc::clone(&steps);
        Self {
            thread: thread::spawn(move || {
                let _lifetime = lifetime;
                work(owned, control)
            }),
            receipt,
            steps,
        }
    }

    /// Whether the host thread has completed, including release.
    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }

    /// The quit disposition selected so far, independent of presentation.
    /// Only the host can record escalation; an elapsed frontend deadline cannot.
    pub fn quit_outcome(&self) -> HostQuitOutcome {
        self.receipt.outcome()
    }

    /// Joins cleanup and returns its typed quit disposition.
    pub fn join(self) -> thread::Result<CliResult<HostQuitOutcome>> {
        self.thread
            .join()
            .map(|result| result.map(|()| self.receipt.outcome()))
    }
}

#[cfg(test)]
mod tests;
