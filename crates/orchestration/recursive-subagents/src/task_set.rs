//! Owned sets of background tasks.
//!
//! Every task a hosted session or an agent tree spawns goes through its
//! owner's [`TaskSet`], so the owner's end can abort and join all of them
//! instead of relying on an executor being dropped. A session owns one set
//! and a tree owns its own, which ends with the tree's last root lease: a
//! root that releases or is succeeded never aborts the tree's children. A
//! handle is cheap to clone; every clone names the same set.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::task::{AbortHandle, JoinHandle, JoinSet};
use tokio_util::task::TaskTracker;
use tokio_util::task::task_tracker::TrackedFuture;

/// One owner's set of spawned tasks.
#[derive(Clone)]
pub struct TaskSet {
    inner: Arc<Inner>,
}

struct Inner {
    tracker: TaskTracker,
    /// Abort handles of tasks that may still run; `None` once closed.
    running: Mutex<Option<Vec<AbortHandle>>>,
}

impl TaskSet {
    /// An open, empty set.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                tracker: TaskTracker::new(),
                running: Mutex::new(Some(Vec::new())),
            }),
        }
    }

    /// Spawns `future` on the current Tokio runtime as a task of this set.
    ///
    /// A closed set still returns a handle, but the task is aborted before it
    /// first runs, as a task spawned onto a runtime that is shutting down is.
    ///
    /// # Panics
    ///
    /// Panics when called outside a Tokio runtime, as `tokio::spawn` does.
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.register(future, |tracked| {
            let task = tokio::spawn(tracked);
            let abort = task.abort_handle();
            (task, abort)
        })
    }

    /// Spawns `future` into a caller's `JoinSet` and also into this set, so
    /// the caller keeps joining it and this set can still abort it.
    pub(crate) fn spawn_into<F>(&self, set: &mut JoinSet<F::Output>, future: F) -> AbortHandle
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.register(future, |tracked| {
            let abort = set.spawn(tracked);
            (abort.clone(), abort)
        })
    }

    fn register<F: Future, R>(
        &self,
        future: F,
        spawn: impl FnOnce(TrackedFuture<F>) -> (R, AbortHandle),
    ) -> R {
        // Spawning under the lock orders every spawn before or after a close.
        let mut running = self.lock();
        let (spawned, abort) = spawn(self.inner.tracker.track_future(future));
        match running.as_mut() {
            Some(running) => {
                running.retain(|task| !task.is_finished());
                running.push(abort);
            }
            None => abort.abort(),
        }
        spawned
    }

    /// Closes the set and aborts every task in it. Later spawns are aborted
    /// at once. Idempotent.
    pub fn abort_all(&self) {
        let running = self.lock().take();
        self.inner.tracker.close();
        for task in running.into_iter().flatten() {
            task.abort();
        }
    }

    /// Aborts every task, then waits up to `bound` until each one has
    /// finished or dropped its future. Returns whether all of them did.
    pub async fn close(&self, bound: Duration) -> bool {
        self.abort_all();
        tokio::time::timeout(bound, self.inner.tracker.wait())
            .await
            .is_ok()
    }

    /// Whether [`Self::abort_all`] or [`Self::close`] has run.
    pub fn is_closed(&self) -> bool {
        self.lock().is_none()
    }

    /// How many tasks of this set have not yet finished or been dropped.
    pub fn len(&self) -> usize {
        self.inner.tracker.len()
    }

    /// Whether every task of this set has finished or been dropped.
    pub fn is_empty(&self) -> bool {
        self.inner.tracker.is_empty()
    }

    fn lock(&self) -> MutexGuard<'_, Option<Vec<AbortHandle>>> {
        self.inner
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for TaskSet {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for TaskSet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TaskSet")
            .field("len", &self.len())
            .field("closed", &self.is_closed())
            .finish()
    }
}

#[cfg(test)]
mod tests;
