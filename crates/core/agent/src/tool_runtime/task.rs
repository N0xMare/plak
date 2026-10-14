//! Object-safe, agent-owned supervision for dispatched executor futures.

use std::collections::BTreeMap;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::sync::watch;

use plakous_tools::{ToolOutcome, ToolRuntimeError, ToolRuntimeErrorKind};

use super::dispatch_error::{DispatchFailure, ToolDispatchError};

/// Owned executor task accepted by a [`ToolTaskOwner`].
pub type ToolTaskFuture =
    Pin<Box<dyn Future<Output = Result<ToolOutcome, ToolRuntimeError>> + Send + 'static>>;

/// Owned future observing one supervised task's terminal result.
pub type ToolTaskJoin =
    Pin<Box<dyn Future<Output = Result<ToolOutcome, ToolRuntimeError>> + Send + 'static>>;

/// Owned future completing after an owner has no active tasks.
pub type ToolTaskDrain = Pin<Box<dyn Future<Output = ToolTaskOwnerStatus> + Send + 'static>>;

/// Point-in-time state of one supervised task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ToolTaskStatus {
    /// The task is running and no abort was requested.
    Running,
    /// Abort was requested but the executor future has not ended yet.
    AbortRequested,
    /// The task produced, panicked, was aborted, or was otherwise joined terminally.
    Completed,
}

/// Bounded owner-level task status.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ToolTaskOwnerStatus {
    active: usize,
    abort_requested: usize,
    closed: bool,
}

impl ToolTaskOwnerStatus {
    /// Creates a status snapshot for an injected task owner.
    #[must_use]
    pub const fn new(active: usize, abort_requested: usize) -> Self {
        Self {
            active,
            abort_requested,
            closed: false,
        }
    }

    /// Marks whether the snapshot belongs to an admission-closed owner.
    #[must_use]
    pub const fn with_closed(mut self, closed: bool) -> Self {
        self.closed = closed;
        self
    }

    /// Returns tasks that have not reached terminal observation.
    #[must_use]
    pub const fn active(self) -> usize {
        self.active
    }

    /// Returns active tasks for which abort was requested.
    #[must_use]
    pub const fn abort_requested(self) -> usize {
        self.abort_requested
    }

    /// Reports whether the owner rejects all future task admission.
    #[must_use]
    pub const fn is_closed(self) -> bool {
        self.closed
    }

    /// Reports whether all supervised work has ended.
    #[must_use]
    pub const fn is_drained(self) -> bool {
        self.active == 0
    }
}

/// Object-safe handle for one supervised task.
pub trait ToolTask: Send + Sync + 'static {
    /// Returns a future observing the terminal task result.
    fn join(&self) -> ToolTaskJoin;

    /// Requests executor-future abort.
    ///
    /// Abort is cooperative at the async runtime boundary. It drops the future
    /// when Tokio next regains control; it cannot preempt a hostile in-process
    /// poll that blocks the executor thread.
    fn request_abort(&self);

    /// Returns the current task state.
    fn status(&self) -> ToolTaskStatus;
}

/// Shared object-safe task handle.
pub type ToolTaskHandle = Arc<dyn ToolTask>;

/// Object-safe owner that tracks dispatched work independently of its caller.
pub trait ToolTaskOwner: Send + Sync + 'static {
    /// Starts one owned task. Dropping the returned handle must not abandon or
    /// cancel the task because the owner retains it until completion.
    ///
    /// # Errors
    ///
    /// Returns an execution error when the owner cannot start the task.
    fn spawn(&self, future: ToolTaskFuture) -> Result<ToolTaskHandle, ToolRuntimeError>;

    /// Returns an aggregate status snapshot.
    fn status(&self) -> ToolTaskOwnerStatus;

    /// Requests abort for every currently tracked task and returns the count.
    fn request_abort_all(&self) -> usize;

    /// Returns a future completing when the tasks currently owned have reached
    /// terminal observation. This does not close admission.
    fn drain(&self) -> ToolTaskDrain;

    /// Atomically closes admission, requests abort for every active task, and
    /// returns a future completing at a stable drained boundary.
    fn shutdown(&self) -> ToolTaskDrain;
}

/// Tokio-backed tracked task owner.
#[derive(Clone, Debug)]
pub struct TokioToolTaskOwner {
    inner: Arc<TokioTaskOwnerInner>,
}

#[derive(Debug)]
struct TokioTaskOwnerInner {
    state: Mutex<OwnerState>,
    updates: watch::Sender<u64>,
}

#[derive(Debug, Default)]
struct OwnerState {
    next_id: u64,
    active: BTreeMap<u64, Arc<TokioTask>>,
    closed: bool,
}

impl Default for TokioToolTaskOwner {
    fn default() -> Self {
        Self::new()
    }
}

impl TokioToolTaskOwner {
    /// Creates an empty Tokio task owner. Dispatch requires an active runtime.
    #[must_use]
    pub fn new() -> Self {
        let (updates, _) = watch::channel(0);
        Self {
            inner: Arc::new(TokioTaskOwnerInner {
                state: Mutex::new(OwnerState::default()),
                updates,
            }),
        }
    }
}

impl ToolTaskOwner for TokioToolTaskOwner {
    fn spawn(&self, future: ToolTaskFuture) -> Result<ToolTaskHandle, ToolRuntimeError> {
        let handle = tokio::runtime::Handle::try_current().map_err(|_| {
            ToolDispatchError::new(
                ToolRuntimeErrorKind::Execution,
                "tool dispatch requires an active Tokio runtime",
            )
            .into_portable()
        })?;
        let task = Arc::new(TokioTask::new());
        let id = {
            let mut owner = lock(&self.inner.state);
            if owner.closed {
                return Err(ToolDispatchError::new(
                    ToolRuntimeErrorKind::Execution,
                    "tool task owner is closed",
                )
                .into_portable());
            }
            let id = owner.next_id;
            owner.next_id = owner.next_id.wrapping_add(1);
            owner.active.insert(id, Arc::clone(&task));
            id
        };
        self.inner.notify();

        let Ok(worker) = catch_unwind(AssertUnwindSafe(|| handle.spawn(future))) else {
            self.inner.complete(id);
            return Err(spawn_error());
        };
        task.install_abort(&worker.abort_handle());
        let worker = AbortOnDropJoin { worker };
        let completion = CompletionGuard {
            task: Arc::clone(&task),
            owner: Arc::clone(&self.inner),
            id,
            result: None,
        };
        let supervisor = async move {
            let result = match worker.await {
                Ok(result) => result.map_err(DispatchFailure::Executor),
                Err(error) if error.is_panic() => Err(ToolDispatchError::new(
                    ToolRuntimeErrorKind::Execution,
                    "supervised tool task panicked before producing a terminal result",
                )
                .into()),
                Err(error) if error.is_cancelled() => Err(ToolDispatchError::new(
                    ToolRuntimeErrorKind::Execution,
                    "supervised tool task was aborted before producing a terminal result",
                )
                .into()),
                Err(_) => Err(ToolDispatchError::new(
                    ToolRuntimeErrorKind::Execution,
                    "supervised tool task failed before producing a terminal result",
                )
                .into()),
            };
            completion.finish(result.map_err(DispatchFailure::into_portable));
        };
        if catch_unwind(AssertUnwindSafe(|| handle.spawn(supervisor))).is_err() {
            return Err(spawn_error());
        }
        Ok(Arc::new(TokioTaskHandle { task }))
    }

    fn status(&self) -> ToolTaskOwnerStatus {
        self.inner.status()
    }

    fn request_abort_all(&self) -> usize {
        let tasks = {
            let owner = lock(&self.inner.state);
            owner.active.values().cloned().collect::<Vec<_>>()
        };
        for task in &tasks {
            task.request_abort();
        }
        tasks.len()
    }

    fn drain(&self) -> ToolTaskDrain {
        owner_drain(Arc::clone(&self.inner))
    }

    fn shutdown(&self) -> ToolTaskDrain {
        let tasks = {
            let mut owner = lock(&self.inner.state);
            owner.closed = true;
            owner.active.values().cloned().collect::<Vec<_>>()
        };
        self.inner.notify();
        for task in tasks {
            task.request_abort();
        }
        owner_drain(Arc::clone(&self.inner))
    }
}

#[derive(Debug)]
struct TokioTask {
    state: Mutex<TaskState>,
    abort_requested: AtomicBool,
    completed: AtomicBool,
    completion: watch::Sender<bool>,
}

#[derive(Debug, Default)]
struct TaskState {
    result: Option<Result<ToolOutcome, ToolRuntimeError>>,
    abort: Option<tokio::task::AbortHandle>,
}

impl TokioTask {
    fn new() -> Self {
        let (completion, _) = watch::channel(false);
        Self {
            state: Mutex::new(TaskState::default()),
            abort_requested: AtomicBool::new(false),
            completed: AtomicBool::new(false),
            completion,
        }
    }

    fn install_abort(&self, abort: &tokio::task::AbortHandle) {
        let requested = self.abort_requested.load(Ordering::Acquire);
        lock(&self.state).abort = Some(abort.clone());
        if requested {
            abort.abort();
        }
    }

    fn complete(&self, result: Result<ToolOutcome, ToolRuntimeError>) {
        let mut state = lock(&self.state);
        if state.result.is_none() {
            state.result = Some(result);
            state.abort = None;
            self.completed.store(true, Ordering::Release);
        }
        drop(state);
        self.completion.send_replace(true);
    }
}

impl TokioTask {
    fn request_abort(&self) {
        if self.completed.load(Ordering::Acquire) {
            return;
        }
        self.abort_requested.store(true, Ordering::Release);
        if let Some(abort) = lock(&self.state).abort.clone() {
            abort.abort();
        }
    }

    fn status(&self) -> ToolTaskStatus {
        if self.completed.load(Ordering::Acquire) {
            ToolTaskStatus::Completed
        } else if self.abort_requested.load(Ordering::Acquire) {
            ToolTaskStatus::AbortRequested
        } else {
            ToolTaskStatus::Running
        }
    }
}

#[derive(Debug)]
struct TokioTaskHandle {
    task: Arc<TokioTask>,
}

impl ToolTask for TokioTaskHandle {
    fn join(&self) -> ToolTaskJoin {
        task_join(Arc::clone(&self.task))
    }

    fn request_abort(&self) {
        self.task.request_abort();
    }

    fn status(&self) -> ToolTaskStatus {
        self.task.status()
    }
}

fn task_join(task: Arc<TokioTask>) -> ToolTaskJoin {
    let mut completion = task.completion.subscribe();
    Box::pin(async move {
        loop {
            if let Some(result) = lock(&task.state).result.clone() {
                return result;
            }
            if completion.changed().await.is_err() {
                return Err(ToolDispatchError::new(
                    ToolRuntimeErrorKind::Execution,
                    "tool task completion channel closed before terminal observation",
                )
                .into_portable());
            }
        }
    })
}

fn owner_drain(owner: Arc<TokioTaskOwnerInner>) -> ToolTaskDrain {
    let mut updates = owner.updates.subscribe();
    Box::pin(async move {
        loop {
            let status = owner.status();
            if status.is_drained() {
                return status;
            }
            if updates.changed().await.is_err() {
                return owner.status();
            }
        }
    })
}

impl TokioTaskOwnerInner {
    fn status(&self) -> ToolTaskOwnerStatus {
        let owner = lock(&self.state);
        ToolTaskOwnerStatus {
            active: owner.active.len(),
            abort_requested: owner
                .active
                .values()
                .filter(|task| task.status() == ToolTaskStatus::AbortRequested)
                .count(),
            closed: owner.closed,
        }
    }

    fn complete(&self, id: u64) {
        lock(&self.state).active.remove(&id);
        self.notify();
    }

    fn notify(&self) {
        self.updates.send_modify(|revision| {
            *revision = revision.wrapping_add(1);
        });
    }
}

struct CompletionGuard {
    task: Arc<TokioTask>,
    owner: Arc<TokioTaskOwnerInner>,
    id: u64,
    result: Option<Result<ToolOutcome, ToolRuntimeError>>,
}

impl CompletionGuard {
    fn finish(mut self, result: Result<ToolOutcome, ToolRuntimeError>) {
        self.result = Some(result);
    }
}

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        let result = self.result.take().unwrap_or_else(|| {
            Err(ToolDispatchError::new(
                ToolRuntimeErrorKind::Execution,
                "tool task supervision ended before terminal observation",
            )
            .into_portable())
        });
        self.task.complete(result);
        self.owner.complete(self.id);
    }
}

struct AbortOnDropJoin {
    worker: tokio::task::JoinHandle<Result<ToolOutcome, ToolRuntimeError>>,
}

impl Future for AbortOnDropJoin {
    type Output = Result<Result<ToolOutcome, ToolRuntimeError>, tokio::task::JoinError>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.worker).poll(context)
    }
}

impl Drop for AbortOnDropJoin {
    fn drop(&mut self) {
        if !self.worker.is_finished() {
            self.worker.abort();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn spawn_error() -> ToolRuntimeError {
    ToolDispatchError::new(
        ToolRuntimeErrorKind::Execution,
        "tool task supervisor could not start its worker",
    )
    .into_portable()
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn abort_is_observable_and_owner_drains() -> Result<(), ToolRuntimeError> {
        let owner = TokioToolTaskOwner::new();
        let task = owner.spawn(Box::pin(std::future::pending()))?;
        assert_eq!(owner.status().active(), 1);
        task.request_abort();
        assert_eq!(task.status(), ToolTaskStatus::AbortRequested);
        let Err(error) = task.join().await else {
            panic!("aborted task must fail");
        };
        assert_eq!(error.kind(), ToolRuntimeErrorKind::Execution);
        assert!(owner.drain().await.is_drained());
        Ok(())
    }

    #[tokio::test]
    async fn async_panic_is_joined_and_observed() -> Result<(), ToolRuntimeError> {
        let owner = TokioToolTaskOwner::new();
        let task = owner.spawn(Box::pin(async {
            panic!("intentional task panic");
            #[allow(
                unreachable_code,
                reason = "the expression fixes the async output type"
            )]
            ToolOutcome::success_text("unreachable")
        }))?;
        let Err(error) = task.join().await else {
            panic!("panic must become an error");
        };
        assert_eq!(error.kind(), ToolRuntimeErrorKind::Execution);
        assert!(owner.drain().await.is_drained());
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_join_and_drain_waiters_all_observe_completion()
    -> Result<(), ToolRuntimeError> {
        let owner = TokioToolTaskOwner::new();
        let task = owner.spawn(Box::pin(std::future::pending()))?;
        let first_join = task.join();
        let second_join = task.join();
        let first_drain = owner.drain();
        let second_drain = owner.drain();
        task.request_abort();
        let (first, second, first_status, second_status) =
            tokio::join!(first_join, second_join, first_drain, second_drain);
        assert!(first.is_err());
        assert!(second.is_err());
        assert!(first_status.is_drained());
        assert!(second_status.is_drained());
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_closes_admission_aborts_and_is_clone_idempotent()
    -> Result<(), ToolRuntimeError> {
        let owner = TokioToolTaskOwner::new();
        let clone = owner.clone();
        let task = owner.spawn(Box::pin(std::future::pending()))?;
        let shutdown = owner.shutdown();
        assert!(owner.status().is_closed());
        assert!(clone.spawn(Box::pin(std::future::pending())).is_err());
        assert!(shutdown.await.is_drained());
        assert!(task.join().await.is_err());
        let repeated = clone.shutdown().await;
        assert!(repeated.is_closed());
        assert!(repeated.is_drained());
        Ok(())
    }
}
