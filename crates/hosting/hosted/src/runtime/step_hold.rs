//! Reversible host-owner rendezvous; runtime and custody stay with their owners.

use crate::topology_runtime::ProductTopologyRuntime;
use crate::{CliFailure, CliResult};
use plakous::agent::{Agent, WeakAgent};
use plakous::model::TurnId;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

#[derive(Clone)]
pub(crate) struct OwnerBoundary {
    pub(crate) agent: WeakAgent,
    pub(crate) turn: Option<TurnId>,
    pub(crate) tree: Option<Arc<ProductTopologyRuntime>>,
}

#[derive(Default)]
struct State {
    sequence: u64,
    generation: Option<u64>,
    boundary: Option<OwnerBoundary>,
    observed_agent: Option<WeakAgent>,
    interrupted: bool,
    closed: bool,
}

#[derive(Default)]
pub(crate) struct StepControl {
    state: Mutex<State>,
    requested: AtomicBool,
    pub(crate) changed: tokio::sync::Notify,
}

impl std::fmt::Debug for StepControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StepControl")
            .field("requested", &self.requested())
            .finish_non_exhaustive()
    }
}

impl StepControl {
    pub(crate) fn observe_agent(&self, agent: Option<&Agent>) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .observed_agent = agent.map(Agent::downgrade);
    }
    pub(crate) fn requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    pub(crate) fn pause(self: &Arc<Self>) -> CliResult<WorkerStepPause> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.closed || state.generation.is_some() {
            return Err(CliFailure::new(
                "the session owner is already held or stopping",
            ));
        }
        state.sequence = state
            .sequence
            .checked_add(1)
            .ok_or_else(|| CliFailure::new("session hold generation exhausted"))?;
        let generation = state.sequence;
        state.generation = Some(generation);
        state.boundary = None;
        state.interrupted = false;
        self.requested.store(true, Ordering::Release);
        self.changed.notify_waiters();
        Ok(WorkerStepPause {
            control: Arc::clone(self),
            generation,
        })
    }

    /// Called only at the root owner's joined host-operation boundary.
    pub(crate) fn owner_boundary(
        &self,
        agent: Option<&Agent>,
        turn: Option<TurnId>,
        tree: Option<Arc<ProductTopologyRuntime>>,
    ) {
        if !self.requested() {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.generation.is_some() {
            state.boundary = agent.map(|agent| OwnerBoundary {
                agent: agent.downgrade(),
                turn,
                tree,
            });
        }
    }

    fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .closed = true;
        self.changed.notify_waiters();
    }

    pub(crate) fn interrupt(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.generation.is_some() {
            state.interrupted = true;
            self.changed.notify_waiters();
        }
    }
}

pub(crate) struct WorkerStepPause {
    control: Arc<StepControl>,
    generation: u64,
}

impl WorkerStepPause {
    pub(crate) fn observed_agent(&self) -> CliResult<Option<Agent>> {
        let state = self
            .control
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if state.closed || state.generation != Some(self.generation) {
            return Err(CliFailure::new(
                "the observed session owner ended before its hold",
            ));
        }
        if state.interrupted {
            return Err(CliFailure::new(
                "host_step_drain_interrupted: an operator stop or interrupt takes precedence; source remains authoritative",
            ));
        }
        Ok(state.observed_agent.as_ref().and_then(WeakAgent::upgrade))
    }
    pub(crate) fn boundary(&self) -> CliResult<Option<OwnerBoundary>> {
        let state = self
            .control
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if state.closed || state.generation != Some(self.generation) {
            return Err(CliFailure::new(
                "the held session owner ended before handoff",
            ));
        }
        if state.interrupted {
            return Err(CliFailure::new(
                "host_step_drain_interrupted: an operator stop or interrupt takes precedence; source remains authoritative",
            ));
        }
        Ok(state.boundary.clone())
    }
}

impl Drop for WorkerStepPause {
    fn drop(&mut self) {
        let mut state = self
            .control
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if state.generation == Some(self.generation) {
            state.generation = None;
            state.boundary = None;
            self.control.requested.store(false, Ordering::Release);
            self.control.changed.notify_waiters();
        }
    }
}

pub(super) struct StepOwner(pub(super) Arc<StepControl>);

impl Drop for StepOwner {
    fn drop(&mut self) {
        self.0.close();
    }
}
