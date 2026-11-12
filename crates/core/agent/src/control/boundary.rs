//! Process-local host coordination, never a turn or durability authority.

use plakous_model::{CheckpointId, TurnId};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::{Notify, watch};

/// Definite process-local refusals; no command or durable admission occurred.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum BoundaryError {
    /// A different coordinator already owns this driver's hold.
    #[error("this driver already has an active boundary hold")]
    Busy,
    /// The driver exited or cannot issue another generation.
    #[error("the boundary owner is no longer available")]
    Unavailable,
}

/// Operation the sole driver must finish before acknowledging a hold.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundaryPhase {
    /// A generation, including its retries, must settle.
    Model,
    /// A joined paid workflow must finish; the hold never cancels it.
    Compaction,
    /// A tool batch, including read-only groups and result folding, must join.
    Effect,
}

/// Observation of one generation of a host-layer step hold.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundaryProgress {
    /// The owner has not yet acknowledged a between-operation boundary.
    Requested,
    /// Existing work is running; acknowledgement is not a safe boundary.
    Draining {
        /// Active exact turn, or none for idle-session compaction.
        turn: Option<TurnId>,
        /// Existing operation that must settle before holding.
        phase: BoundaryPhase,
    },
    /// The owner is parked before its next operation.
    Held {
        /// Active admitted turn; an idle driver reports none.
        turn: Option<TurnId>,
        /// The owner's current committed checkpoint.
        checkpoint: CheckpointId,
    },
}

/// Revokes its exact generation on drop, even when command ingress is full.
pub struct BoundaryHold {
    state: Arc<BoundaryState>,
    generation: u64,
    progress: watch::Receiver<Result<BoundaryProgress, BoundaryError>>,
}

impl BoundaryHold {
    /// Reads this generation's latest observation without waiting.
    ///
    /// # Errors
    /// Returns unavailable when the owner has exited.
    pub fn progress(&self) -> Result<BoundaryProgress, BoundaryError> {
        *self.progress.borrow()
    }

    /// Waits for the owner to publish a new observation.
    ///
    /// # Errors
    /// Returns unavailable when the owner has exited.
    pub async fn changed(&mut self) -> Result<BoundaryProgress, BoundaryError> {
        self.progress
            .changed()
            .await
            .map_err(|_| BoundaryError::Unavailable)?;
        *self.progress.borrow_and_update()
    }

    /// Waits until the owner has observed this generation's revocation.
    ///
    /// # Errors
    /// Returns unavailable if the owner exits before observing release.
    pub async fn release(self) -> Result<(), BoundaryError> {
        let mut observed = self.state.observed.subscribe();
        let revision = self.state.revoke(self.generation);
        loop {
            if self.state.lock().closed {
                return Err(BoundaryError::Unavailable);
            }
            if *observed.borrow_and_update() >= revision {
                return Ok(());
            }
            observed
                .changed()
                .await
                .map_err(|_| BoundaryError::Unavailable)?;
        }
    }
}

impl Drop for BoundaryHold {
    fn drop(&mut self) {
        self.state.revoke(self.generation);
    }
}

struct Request {
    generation: u64,
    progress: watch::Sender<Result<BoundaryProgress, BoundaryError>>,
}

#[derive(Default)]
struct State {
    revision: u64,
    closed: bool,
    request: Option<Request>,
    activity: Option<(Option<TurnId>, BoundaryPhase)>,
}

pub(crate) struct BoundaryState {
    inner: Mutex<State>,
    pub(crate) changed: Notify,
    observed: watch::Sender<u64>,
}

impl Default for BoundaryState {
    fn default() -> Self {
        Self {
            inner: Mutex::default(),
            changed: Notify::new(),
            observed: watch::channel(0).0,
        }
    }
}

impl BoundaryState {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn request(self: &Arc<Self>) -> Result<BoundaryHold, BoundaryError> {
        let mut state = self.lock();
        if state.closed {
            return Err(BoundaryError::Unavailable);
        }
        if state.request.is_some() {
            return Err(BoundaryError::Busy);
        }
        state.revision = state
            .revision
            .checked_add(1)
            .ok_or(BoundaryError::Unavailable)?;
        let generation = state.revision;
        let initial = state
            .activity
            .map_or(BoundaryProgress::Requested, |(turn, phase)| {
                BoundaryProgress::Draining { turn, phase }
            });
        let (sender, progress) = watch::channel(Ok(initial));
        state.request = Some(Request {
            generation,
            progress: sender,
        });
        self.changed.notify_one();
        Ok(BoundaryHold {
            state: Arc::clone(self),
            generation,
            progress,
        })
    }

    fn revoke(&self, generation: u64) -> u64 {
        let mut state = self.lock();
        if state
            .request
            .as_ref()
            .is_some_and(|request| request.generation == generation)
        {
            state.request = None;
            state.revision = state.revision.saturating_add(1);
            self.changed.notify_one();
        }
        state.revision
    }

    pub(crate) fn requested(&self) -> bool {
        self.lock().request.is_some()
    }

    pub(crate) fn boundary(&self, turn: Option<TurnId>, checkpoint: CheckpointId) -> bool {
        let mut state = self.lock();
        state.activity = None;
        self.observed.send_replace(state.revision);
        if let Some(request) = &state.request {
            let _ = request
                .progress
                .send_replace(Ok(BoundaryProgress::Held { turn, checkpoint }));
            true
        } else {
            false
        }
    }

    pub(crate) fn activity(
        self: &Arc<Self>,
        turn: Option<TurnId>,
        phase: BoundaryPhase,
    ) -> BoundaryActivity {
        let mut state = self.lock();
        let previous = state.activity.replace((turn, phase));
        if let Some(request) = &state.request {
            let _ = request
                .progress
                .send_replace(Ok(BoundaryProgress::Draining { turn, phase }));
        }
        BoundaryActivity {
            state: Arc::clone(self),
            previous,
        }
    }

    fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        if let Some(request) = state.request.take() {
            let _ = request
                .progress
                .send_replace(Err(BoundaryError::Unavailable));
        }
        self.observed.send_replace(state.revision);
        self.changed.notify_one();
    }
}

pub(crate) struct BoundaryActivity {
    state: Arc<BoundaryState>,
    previous: Option<(Option<TurnId>, BoundaryPhase)>,
}

impl Drop for BoundaryActivity {
    fn drop(&mut self) {
        self.state.lock().activity = self.previous;
    }
}

/// Only the actor owns this lifetime; cloned endpoints cannot keep it open.
pub(crate) struct BoundaryOwner(pub(crate) Arc<BoundaryState>);

impl Drop for BoundaryOwner {
    fn drop(&mut self) {
        self.0.close();
    }
}
