//! Startup readiness (spec 02 §4.6): the control socket answers Welcome and
//! serves saved reads while the runner placement inventory, the continuation
//! inventory and the first directory projection complete in the background.
//! Until they do, every open, creation and runner-dependent lifecycle change
//! waits here, and an attach is served from saved history.

use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use plakous_host_protocol::wire::{PlacementReason, RequestRefusal, RunnerLabel, RunnerPhase};
use plakous_hosted::error::CliFailure;

/// How long admission that depends on the inventories waits for them before
/// answering the retryable reconciliation refusal.
pub(super) const ADMISSION_WAIT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Eq, PartialEq)]
enum Phase {
    Starting,
    Ready,
    Failed(String),
    Stopped,
}

/// The host's startup phase, shared by the session directory and the start.
pub(crate) struct Startup {
    phase: Mutex<Phase>,
    settled: Condvar,
}

impl Startup {
    pub(super) const fn starting() -> Self {
        Self {
            phase: Mutex::new(Phase::Starting),
            settled: Condvar::new(),
        }
    }

    /// A host whose inventories are already complete: sessions composed
    /// without a host start.
    pub(super) const fn ready() -> Self {
        Self {
            phase: Mutex::new(Phase::Ready),
            settled: Condvar::new(),
        }
    }

    pub(super) fn is_ready(&self) -> bool {
        *self.lock() == Phase::Ready
    }

    pub(super) fn failure(&self) -> Option<String> {
        match &*self.lock() {
            Phase::Failed(message) => Some(message.clone()),
            Phase::Starting | Phase::Ready | Phase::Stopped => None,
        }
    }

    /// Why an open would not be admitted now, without waiting.
    pub(super) fn deferred(&self, runner: &RunnerLabel) -> Option<CliFailure> {
        (!self.is_ready()).then(|| self.refusal(runner))
    }

    /// Waits up to `bound` for the inventories; otherwise the typed,
    /// retryable reconciliation refusal of `runner`.
    pub(super) fn admit(&self, runner: &RunnerLabel, bound: Duration) -> Result<(), CliFailure> {
        let deadline = Instant::now() + bound;
        let mut phase = self.lock();
        while *phase == Phase::Starting {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            phase = self
                .settled
                .wait_timeout(phase, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        if *phase == Phase::Ready {
            return Ok(());
        }
        drop(phase);
        Err(self.refusal(runner))
    }

    fn refusal(&self, runner: &RunnerLabel) -> CliFailure {
        let message = match &*self.lock() {
            Phase::Failed(_) => "the host could not complete its startup inventories",
            Phase::Stopped => "the host is stopping",
            Phase::Starting | Phase::Ready => {
                "the host is still reading its startup inventories; attach again once it reports running"
            }
        };
        CliFailure::new(message).with_wire_refusal(RequestRefusal::PlacementRefused {
            runner: runner.clone(),
            reason: Box::new(PlacementReason::RunnerUnreachable {
                phase: RunnerPhase::Reconcile,
                timed_out: false,
            }),
        })
    }

    /// Admits work once the inventories are complete. `announce` runs under
    /// the phase lock, so a stop cannot publish before it and be overtaken.
    /// False when a stop or a failure came first.
    pub(super) fn complete(&self, announce: impl FnOnce()) -> bool {
        let mut phase = self.lock();
        if *phase != Phase::Starting {
            return false;
        }
        announce();
        *phase = Phase::Ready;
        drop(phase);
        self.settled.notify_all();
        true
    }

    pub(super) fn fail(&self, message: String) {
        self.settle(Phase::Failed(message));
    }

    /// A stop ends an unfinished start: waiting admission is refused, and a
    /// completion that has not been announced yet is not announced at all.
    /// A completed start is left to the host's own stop.
    pub(super) fn stop(&self) {
        self.settle(Phase::Stopped);
    }

    fn settle(&self, next: Phase) {
        let mut phase = self.lock();
        if *phase == Phase::Starting {
            *phase = next;
        }
        drop(phase);
        self.settled.notify_all();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Phase> {
        self.phase.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> RunnerLabel {
        RunnerLabel::local()
    }

    fn reconciling(failure: &CliFailure) -> bool {
        matches!(
            failure.wire_refusal(),
            Some(RequestRefusal::PlacementRefused { reason, .. })
                if matches!(**reason, PlacementReason::RunnerUnreachable {
                    phase: RunnerPhase::Reconcile,
                    timed_out: false,
                })
        )
    }

    #[test]
    fn admission_waits_for_the_inventories_and_then_admits() {
        let startup = std::sync::Arc::new(Startup::starting());
        assert!(startup.deferred(&local()).is_some_and(|f| reconciling(&f)));
        let (entered, waiting) = std::sync::mpsc::channel();
        let waiter = {
            let startup = std::sync::Arc::clone(&startup);
            std::thread::spawn(move || {
                entered.send(()).unwrap();
                startup.admit(&local(), Duration::from_secs(60))
            })
        };
        waiting.recv().unwrap();
        // The waiter cannot be admitted before completion is announced.
        let mut announced = false;
        assert!(startup.complete(|| announced = true));
        assert!(announced);
        assert!(waiter.join().unwrap().is_ok());
        assert!(startup.deferred(&local()).is_none());
        assert!(!startup.complete(|| unreachable!("completion is announced once")));
    }

    #[test]
    fn a_bounded_wait_answers_the_retryable_reconciliation_refusal() {
        let startup = Startup::starting();
        let failure = startup.admit(&local(), Duration::ZERO).unwrap_err();
        assert!(reconciling(&failure));
        assert!(!startup.is_ready());
    }

    #[test]
    fn a_stop_or_failure_refuses_waiters_and_suppresses_the_running_notice() {
        for fail in [false, true] {
            let startup = std::sync::Arc::new(Startup::starting());
            let waiter = {
                let startup = std::sync::Arc::clone(&startup);
                std::thread::spawn(move || startup.admit(&local(), Duration::from_secs(60)))
            };
            if fail {
                startup.fail("inventory unreadable".into());
                assert_eq!(startup.failure().as_deref(), Some("inventory unreadable"));
            } else {
                startup.stop();
            }
            assert!(waiter.join().unwrap().is_err_and(|f| reconciling(&f)));
            assert!(!startup.complete(|| panic!("a stopped start announces nothing")));
        }
    }
}
