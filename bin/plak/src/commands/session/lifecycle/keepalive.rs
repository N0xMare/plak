//! Keep non-executing branch fences alive through synchronous product-store commits.

use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::Duration;

use plakous::application::ApplicationExecutor;
use plakous::sessions::BranchHold;
use plakous_hosted::error::CliFailure;

use super::LifecycleError;

#[derive(Default)]
struct State {
    holds: Vec<BranchHold>,
    failure: Option<String>,
}

pub(super) struct Keepalive {
    state: Arc<Mutex<State>>,
    stop: mpsc::Sender<()>,
    worker: Option<std::thread::JoinHandle<()>>,
}

#[cfg(test)]
fn renewal_failure_fixture() -> bool {
    std::env::var("PLAK_LIFECYCLE_FAIL_RENEWAL").as_deref() == Ok("1")
}

fn renew(state: &mut State, executor: &ApplicationExecutor) -> Result<(), LifecycleError> {
    if let Some(failure) = &state.failure {
        return Err(CliFailure::new(failure.clone()).into());
    }
    for held in &mut state.holds {
        #[cfg(test)]
        if renewal_failure_fixture() {
            state.failure = Some("injected branch renewal refusal".into());
            return Err(CliFailure::new("injected branch renewal refusal").into());
        }
        let result = executor
            .block_on(held.renew())
            .map_err(|_| "branch renewal executor failed".to_owned())
            .and_then(|result| result.map_err(|error| format!("branch renewal failed: {error}")));
        if let Err(failure) = result {
            state.failure = Some(failure.clone());
            return Err(CliFailure::new(failure).into());
        }
    }
    Ok(())
}

impl Keepalive {
    pub(super) fn new(lease_seconds: u64) -> Result<Self, LifecycleError> {
        let executor = ApplicationExecutor::new()
            .map_err(|_| CliFailure::new("branch renewal executor unavailable"))?;
        let period = Duration::from_millis(lease_seconds.saturating_mul(1000) / 4);
        let state = Arc::new(Mutex::new(State::default()));
        let shared = state.clone();
        let (stop, receiver) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("session-lifecycle-leases".into())
            .spawn(move || {
                while let Err(mpsc::RecvTimeoutError::Timeout) = receiver.recv_timeout(period) {
                    let mut state = shared.lock().unwrap_or_else(PoisonError::into_inner);
                    let _ = renew(&mut state, &executor);
                }
                let holds = std::mem::take(
                    &mut shared.lock().unwrap_or_else(PoisonError::into_inner).holds,
                );
                for held in holds.into_iter().rev() {
                    let _ = executor.block_on(held.release());
                }
            })
            .map_err(|error| CliFailure::new(format!("branch renewal worker failed: {error}")))?;
        Ok(Self {
            state,
            stop,
            worker: Some(worker),
        })
    }

    pub(super) fn add(&self, hold: BranchHold) -> Result<(), LifecycleError> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(failure) = &state.failure {
            // Keep custody of the newly acquired hold until Drop releases it.
            let failure = failure.clone();
            state.holds.push(hold);
            return Err(CliFailure::new(failure).into());
        }
        state.holds.push(hold);
        Ok(())
    }

    pub(super) fn verify(&self, executor: &ApplicationExecutor) -> Result<(), LifecycleError> {
        if self
            .worker
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
        {
            return Err(CliFailure::new("branch renewal worker stopped before commit").into());
        }
        renew(
            &mut self.state.lock().unwrap_or_else(PoisonError::into_inner),
            executor,
        )
    }
}

impl Drop for Keepalive {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
