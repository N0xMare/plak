//! Reversible host-start admission; dispatched tool effects still join.

use super::{ProductTopologyRuntime, Residency};
use plakous::agent::Agent;
use std::sync::{Arc, atomic::Ordering};

/// Defers host starts and recovery; dispatched tool effects still join normally.
pub struct TreeStepPause(Arc<ProductTopologyRuntime>);

/// Already-admitted runtimes and their still-owned outer transitions.
pub struct TreeStepSnapshot {
    /// Existing drivers to hold before their next dispatch.
    pub agents: Vec<Agent>,
    /// Startup, release or terminal projection is still in flight.
    pub transitioning: bool,
}

impl ProductTopologyRuntime {
    /// Retains a host recovery retry without turning a pause into durable debt.
    pub fn defer_step_recovery(&self) {
        self.step_recovery_due.store(true, Ordering::Release);
        if !self.is_closed()
            && !self.step_paused.load(Ordering::Acquire)
            && self.step_recovery_due.swap(false, Ordering::AcqRel)
        {
            self.driver().request_retirement_recovery();
        }
    }
    /// Waits for a reversible host pause to release, without admitting work.
    ///
    /// # Errors
    /// The caller's deadline expired while the tree was still held.
    pub async fn wait_step_release(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<(), super::RecoveryClaimRefusal> {
        loop {
            let changed = self.step_released.notified();
            if self.is_closed() || !self.step_paused.load(Ordering::Acquire) {
                return Ok(());
            }
            tokio::time::timeout_at(deadline, changed)
                .await
                .map_err(|_| super::RecoveryClaimRefusal::StepHeld)?;
        }
    }
    /// Defers host wakes and recovery under the existing admission locks.
    ///
    /// # Errors
    /// Refuses a closed tree or another coordinator's active pause.
    pub async fn pause_step_admissions(self: &Arc<Self>) -> Result<TreeStepPause, String> {
        let _live = self.live.lock().await;
        let _claims = self
            .recovering
            .lock()
            .map_err(|_| "tree recovery admission is unavailable")?;
        if self.is_closed() || self.step_paused.swap(true, Ordering::AcqRel) {
            return Err("the tree is already held or stopping".to_owned());
        }
        Ok(TreeStepPause(Arc::clone(self)))
    }
}

impl TreeStepPause {
    /// Snapshots admitted drivers only after startup/release transitions join.
    /// A terminal driver still awaiting its tree projection is not quiescent.
    ///
    /// # Errors
    /// Refuses a resident whose runtime identity cannot be inspected.
    pub async fn agents(&self) -> Result<TreeStepSnapshot, String> {
        let live = self.0.live.lock().await;
        let mut agents = Vec::new();
        let mut transitioning = false;
        for (id, child) in &live.nodes {
            let Residency::Resident(child) = child else {
                transitioning = true;
                continue;
            };
            let agent = child
                .agent()
                .ok_or("a resident child has no inspectable runtime")?;
            if agent.turn_control(child.turn_id()).is_none()
                && !self
                    .0
                    .read(|tree| tree.is_parked(id))
                    .map_err(|error| error.message)?
            {
                transitioning = true;
                continue;
            }
            agents.push(agent);
        }
        Ok(TreeStepSnapshot {
            agents,
            transitioning,
        })
    }

    /// Whether admitted workspace effects and recovery owners have joined.
    #[must_use]
    pub fn effects_joined(&self) -> bool {
        self.0.driver().step_effects_joined()
            && self
                .0
                .recovering
                .lock()
                .is_ok_and(|claims| claims.is_empty())
            && self
                .0
                .recycling
                .lock()
                .is_ok_and(|claims| claims.is_empty())
    }
}

impl Drop for TreeStepPause {
    fn drop(&mut self) {
        self.0.step_paused.store(false, Ordering::Release);
        self.0.step_released.notify_waiters();
        if self.0.step_recovery_due.swap(false, Ordering::AcqRel) && !self.0.is_closed() {
            self.0.driver().request_retirement_recovery();
        }
    }
}

#[cfg(test)]
mod tests;
