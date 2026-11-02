//! Owned sets of background tasks.
//!
//! Every task a hosted session or an agent tree spawns goes through its
//! owner's [`TaskSet`], so the owner's end can abort and join all of them
//! instead of relying on an executor being dropped. A session owns one set
//! and a tree owns its own, which ends with the tree's last root lease: a
//! root that releases or is succeeded never aborts the tree's children. A
//! handle is cheap to clone; every clone names the same set. Tasks start
//! through the set's [`Runtime`], never through an executor directly.

use std::fmt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::FutureExt as _;
use plakous::runtime::{Runtime, SpawnedTask, TokioRuntime};
use tokio_util::task::TaskTracker;

/// One owner's set of spawned tasks.
#[derive(Clone)]
pub struct TaskSet {
    inner: Arc<Inner>,
}

struct Inner {
    runtime: Arc<dyn Runtime>,
    tracker: TaskTracker,
    /// Tasks that may still run; `None` once closed.
    running: Mutex<Option<Vec<Arc<dyn SpawnedTask>>>>,
}

impl TaskSet {
    /// An open, empty set on the process's native runtime.
    #[must_use]
    pub fn new() -> Self {
        Self::on(TokioRuntime::shared())
    }

    /// An open, empty set whose tasks start on `runtime`.
    #[must_use]
    pub fn on(runtime: Arc<dyn Runtime>) -> Self {
        Self {
            inner: Arc::new(Inner {
                runtime,
                tracker: TaskTracker::new(),
                running: Mutex::new(Some(Vec::new())),
            }),
        }
    }

    /// Spawns `future` as a task of this set.
    ///
    /// A closed set still returns a handle, but never starts the task: the
    /// handle reports it cancelled, as for a task spawned onto a runtime that
    /// is shutting down.
    ///
    /// # Panics
    ///
    /// The native runtime panics when called outside a Tokio runtime, as
    /// `tokio::spawn` does.
    pub fn spawn<F>(&self, future: F) -> TaskHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (sender, output) = tokio::sync::oneshot::channel();
        // A panic is the task's outcome, as it is for a Tokio task.
        let tracked = self
            .inner
            .tracker
            .track_future(AssertUnwindSafe(future).catch_unwind());
        let finished = Arc::new(AtomicBool::new(false));
        let ended = Arc::clone(&finished);
        let task = async move {
            let mut tracked = Box::pin(tracked);
            let outcome = (&mut tracked).await;
            // Finished before the future and what it holds drop, as a Tokio
            // task is: a waiter woken by that drop sees the task finished.
            ended.store(true, Ordering::Release);
            // Dropped before the output is sent, so a joined handle never
            // sees its task still counted by the set.
            drop(tracked);
            let _ = sender.send(outcome.map_err(|_| TaskJoinError::Panicked));
        };
        // Spawning under the lock orders every spawn before or after a close.
        let mut running = self.lock();
        let Some(running) = running.as_mut() else {
            // A closed set never starts the task; dropping it cancels the handle.
            drop(task);
            return TaskHandle {
                output,
                control: Control {
                    task: Arc::new(NeverStarted),
                    finished,
                },
            };
        };
        let control: Arc<dyn SpawnedTask> = Arc::from(self.inner.runtime.spawn(Box::pin(task)));
        running.retain(|task| !task.is_finished());
        running.push(Arc::clone(&control));
        TaskHandle {
            output,
            control: Control {
                task: control,
                finished,
            },
        }
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
        let joined = Box::pin(self.inner.tracker.wait());
        let expired = self.inner.runtime.sleep(bound);
        matches!(
            futures_util::future::select(joined, expired).await,
            futures_util::future::Either::Left(_)
        )
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

    fn lock(&self) -> MutexGuard<'_, Option<Vec<Arc<dyn SpawnedTask>>>> {
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

impl fmt::Debug for TaskSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TaskSet")
            .field("len", &self.len())
            .field("closed", &self.is_closed())
            .finish()
    }
}

/// A task a closed set refused to start.
struct NeverStarted;

impl SpawnedTask for NeverStarted {
    fn abort(&self) {}

    fn is_finished(&self) -> bool {
        true
    }
}

/// Why a task of a [`TaskSet`] produced no output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskJoinError {
    /// The task was aborted before it finished.
    Cancelled,
    /// The task panicked.
    Panicked,
}

impl TaskJoinError {
    /// Whether the task was aborted before it finished.
    #[must_use]
    pub const fn is_cancelled(self) -> bool {
        matches!(self, Self::Cancelled)
    }

    /// Whether the task panicked.
    #[must_use]
    pub const fn is_panic(self) -> bool {
        matches!(self, Self::Panicked)
    }
}

impl fmt::Display for TaskJoinError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Cancelled => "task was cancelled",
            Self::Panicked => "task panicked",
        })
    }
}

impl std::error::Error for TaskJoinError {}

/// A task of a [`TaskSet`]: await it for its output. Dropping the handle
/// detaches the task; it keeps running.
pub struct TaskHandle<T> {
    output: tokio::sync::oneshot::Receiver<Result<T, TaskJoinError>>,
    control: Control,
}

#[derive(Clone)]
struct Control {
    task: Arc<dyn SpawnedTask>,
    /// Set once the task's future returned, before it is dropped.
    finished: Arc<AtomicBool>,
}

impl Control {
    fn abort(&self) {
        self.task.abort();
    }

    fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire) || self.task.is_finished()
    }
}

impl<T> TaskHandle<T> {
    /// Aborts the task. A task that already finished keeps its output.
    pub fn abort(&self) {
        self.control.abort();
    }

    /// Whether the task has finished, by completing or by being aborted.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.control.is_finished()
    }

    /// A handle that can abort the task without owning its output.
    #[must_use]
    pub fn abort_handle(&self) -> TaskAbortHandle {
        TaskAbortHandle(self.control.clone())
    }
}

impl<T> Future for TaskHandle<T> {
    type Output = Result<T, TaskJoinError>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.output)
            .poll(context)
            .map(|received| received.unwrap_or(Err(TaskJoinError::Cancelled)))
    }
}

impl<T> fmt::Debug for TaskHandle<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TaskHandle")
            .field("finished", &self.is_finished())
            .finish()
    }
}

/// Aborts one task of a [`TaskSet`] without owning its output.
#[derive(Clone)]
pub struct TaskAbortHandle(Control);

impl TaskAbortHandle {
    /// Aborts the task.
    pub fn abort(&self) {
        self.0.abort();
    }

    /// Whether the task has finished, by completing or by being aborted.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.0.is_finished()
    }
}

impl fmt::Debug for TaskAbortHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TaskAbortHandle")
            .field("finished", &self.is_finished())
            .finish()
    }
}

#[cfg(test)]
mod tests;
