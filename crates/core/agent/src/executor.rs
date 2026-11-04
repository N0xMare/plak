//! How agent code starts tasks, waits and reads monotonic time: through the
//! host's [`Runtime`], never through an executor directly.
//!
//! An agent holds one [`Executor`]. Its driver, every task started through it
//! and every host entry point that waits run inside [`Executor::scope`], so
//! code below them reaches it through [`Executor::current`] without carrying
//! it. Outside any scope, `current` is the native Tokio runtime the caller
//! runs on, which is what an agent built without a runtime uses anyway.
//!
//! The agent's own futures are `Send` on every target, so its handle to the
//! runtime, its sleeps and its task handles are too. On `wasm32`, where a
//! runtime's futures need not be `Send`, a sleep runs as a runtime task that
//! signals a `Send` waiter.

use std::fmt;
use std::future::Future;
use std::ops::{Add, AddAssign};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::future::{AbortHandle, Abortable};
use plakous_model::{Runtime, RuntimeBlockingTask, RuntimeSleep, RuntimeTask, SpawnedTask};
use tokio::sync::oneshot;

tokio::task_local! {
    static CURRENT: Executor;
}

/// A runtime the agent can hold from `Send` futures on every target.
pub(crate) type SharedRuntime = Arc<dyn Runtime + Send + Sync>;

/// A sleep the agent can hold across awaits in `Send` futures.
pub(crate) type Sleep = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Delegates to a host runtime handle that is itself `Send` and `Sync`,
/// whatever its trait object says.
struct HostRuntime<R: ?Sized>(Arc<R>);

impl<R: Runtime + Send + Sync + ?Sized> Runtime for HostRuntime<R> {
    fn spawn(&self, task: RuntimeTask) -> Box<dyn SpawnedTask> {
        self.0.spawn(task)
    }

    fn spawn_blocking(&self, work: RuntimeBlockingTask) -> Box<dyn SpawnedTask> {
        self.0.spawn_blocking(work)
    }

    fn sleep(&self, duration: Duration) -> RuntimeSleep {
        self.0.sleep(duration)
    }

    fn now(&self) -> Duration {
        self.0.now()
    }
}

/// One agent's runtime.
#[derive(Clone)]
pub(crate) struct Executor {
    runtime: SharedRuntime,
}

impl fmt::Debug for Executor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Executor")
    }
}

/// A point on one [`Executor`]'s monotonic clock. Points from two runtimes do
/// not compare.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct Instant(Duration);

impl Instant {
    /// [`now`]: this point on the current executor.
    pub(crate) fn now() -> Self {
        now()
    }

    pub(crate) fn saturating_duration_since(self, earlier: Self) -> Duration {
        self.0.saturating_sub(earlier.0)
    }

    /// Time elapsed since this point on the current executor.
    pub(crate) fn elapsed(self) -> Duration {
        now().saturating_duration_since(self)
    }
}

impl Add<Duration> for Instant {
    type Output = Self;

    fn add(self, duration: Duration) -> Self {
        Self(self.0.saturating_add(duration))
    }
}

impl AddAssign<Duration> for Instant {
    fn add_assign(&mut self, duration: Duration) {
        *self = *self + duration;
    }
}

/// A timeout's bound elapsed before its future completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Elapsed;

/// A spawned task's output, or [`Cancelled`] when the task ended without one
/// (aborted, panicked or dropped by its runtime). Dropping the handle
/// detaches the task.
pub(crate) struct JoinHandle<T> {
    abort: AbortHandle,
    output: oneshot::Receiver<T>,
}

/// A task ended without its output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Cancelled;

impl<T> JoinHandle<T> {
    /// Requests that the task stop at its next poll. Awaiting the handle
    /// afterwards completes once the task's future has been dropped or has
    /// finished.
    pub(crate) fn abort(&self) {
        self.abort.abort();
    }

    /// A handle that aborts the task without holding its output.
    pub(crate) fn abort_handle(&self) -> AbortHandle {
        self.abort.clone()
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, Cancelled>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.output)
            .poll(context)
            .map(|output| output.map_err(|_| Cancelled))
    }
}

impl Executor {
    /// An executor over a host runtime handle.
    pub(crate) fn new<R: Runtime + Send + Sync + ?Sized>(runtime: Arc<R>) -> Self {
        Self {
            runtime: Arc::new(HostRuntime(runtime)),
        }
    }

    /// The native default: the Tokio runtime the caller runs on.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn native() -> Option<Self> {
        crate::tokio_runtime::BoundTokioRuntime::current()
            .map(|runtime| Self::new(Arc::new(runtime)))
    }

    /// No native runtime exists on this target; the host supplies one.
    #[cfg(target_arch = "wasm32")]
    pub(crate) const fn native() -> Option<Self> {
        None
    }

    /// The executor of the enclosing [`Self::scope`], else the native one.
    ///
    /// # Panics
    ///
    /// Outside any scope and outside a Tokio runtime, as `tokio::spawn` does.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn current() -> Self {
        CURRENT.try_with(Clone::clone).unwrap_or_else(|_| {
            Self::native().unwrap_or_else(|| Self::new(crate::TokioRuntime::shared()))
        })
    }

    /// The executor of the enclosing [`Self::scope`].
    ///
    /// # Panics
    ///
    /// Outside every scope: this target has no native runtime to fall back on.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn current() -> Self {
        CURRENT
            .try_with(Clone::clone)
            .unwrap_or_else(|_| panic!("agent code ran outside its runtime's scope"))
    }

    /// Runs `future` with this executor as [`Self::current`].
    pub(crate) async fn scope<F: Future>(&self, future: F) -> F::Output {
        CURRENT.scope(self.clone(), future).await
    }

    pub(crate) fn now(&self) -> Instant {
        Instant(self.runtime.now())
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn sleep(&self, duration: Duration) -> Sleep {
        self.runtime.sleep(duration)
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn sleep(&self, duration: Duration) -> Sleep {
        let (elapsed, waiter) = oneshot::channel();
        let sleep = self.runtime.sleep(duration);
        // Detached: a dropped waiter only leaves the timer to finish alone.
        drop(self.runtime.spawn(Box::pin(async move {
            sleep.await;
            let _ = elapsed.send(());
        })));
        Box::pin(async move {
            let _ = waiter.await;
        })
    }

    pub(crate) fn sleep_until(&self, deadline: Instant) -> Sleep {
        self.sleep(deadline.saturating_duration_since(self.now()))
    }

    /// `future`'s output, or [`Elapsed`] once `duration` passes first. The
    /// future is polled before the bound, as Tokio's timeout does.
    pub(crate) async fn timeout<F: Future>(
        &self,
        duration: Duration,
        future: F,
    ) -> Result<F::Output, Elapsed> {
        let expiry = self.sleep(duration);
        tokio::select! {
            biased;
            output = future => Ok(output),
            () = expiry => Err(Elapsed),
        }
    }

    pub(crate) async fn timeout_at<F: Future>(
        &self,
        deadline: Instant,
        future: F,
    ) -> Result<F::Output, Elapsed> {
        let expiry = self.sleep_until(deadline);
        tokio::select! {
            biased;
            output = future => Ok(output),
            () = expiry => Err(Elapsed),
        }
    }

    /// Starts `future` as a task in this executor's scope.
    pub(crate) fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (sender, output) = oneshot::channel();
        let (abort, registration) = AbortHandle::new_pair();
        let scoped = CURRENT.scope(self.clone(), Abortable::new(future, registration));
        // The handle detaches: abort goes through `Abortable`, which drops
        // the future at its next poll on every runtime.
        drop(self.runtime.spawn(Box::pin(async move {
            if let Ok(value) = scoped.await {
                let _ = sender.send(value);
            }
        })));
        JoinHandle { abort, output }
    }

    /// Starts blocking `work` where it does not stall the executor's tasks.
    /// An abort skips work that has not started and does not stop work that
    /// has.
    pub(crate) fn spawn_blocking<F, T>(&self, work: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (sender, output) = oneshot::channel();
        let (abort, _registration) = AbortHandle::new_pair();
        let aborted = abort.clone();
        drop(self.runtime.spawn_blocking(Box::new(move || {
            if !aborted.is_aborted() {
                let _ = sender.send(work());
            }
        })));
        JoinHandle { abort, output }
    }

    /// Ticks every `period`, the first tick at once; a missed tick is skipped
    /// and the next lands on the period's grid, as Tokio's `Skip` does.
    pub(crate) fn interval(&self, period: Duration) -> Interval {
        Interval {
            executor: self.clone(),
            next: self.now(),
            period,
        }
    }
}

/// See [`Executor::interval`].
pub(crate) struct Interval {
    executor: Executor,
    next: Instant,
    period: Duration,
}

impl Interval {
    /// Completes at the next tick. Cancel-safe: dropping it keeps the tick.
    pub(crate) async fn tick(&mut self) {
        self.executor.sleep_until(self.next).await;
        let now = self.executor.now();
        let late = now.saturating_duration_since(self.next).as_nanos();
        let into_period = late % self.period.as_nanos().max(1);
        let into_period = Duration::from_nanos(u64::try_from(into_period).unwrap_or(0));
        self.next = now + self.period.saturating_sub(into_period);
    }
}

/// [`Executor::now`] on the current executor.
pub(crate) fn now() -> Instant {
    Executor::current().now()
}

/// [`Executor::sleep`] on the current executor.
pub(crate) fn sleep(duration: Duration) -> Sleep {
    Executor::current().sleep(duration)
}

/// [`Executor::sleep_until`] on the current executor.
pub(crate) fn sleep_until(deadline: Instant) -> Sleep {
    Executor::current().sleep_until(deadline)
}

/// [`Executor::timeout`] on the current executor.
pub(crate) async fn timeout<F: Future>(
    duration: Duration,
    future: F,
) -> Result<F::Output, Elapsed> {
    Executor::current().timeout(duration, future).await
}

/// [`Executor::timeout_at`] on the current executor.
pub(crate) async fn timeout_at<F: Future>(
    deadline: Instant,
    future: F,
) -> Result<F::Output, Elapsed> {
    Executor::current().timeout_at(deadline, future).await
}

/// [`Executor::spawn_blocking`] on the current executor.
pub(crate) fn spawn_blocking<F, T>(work: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    Executor::current().spawn_blocking(work)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests may assert with unwrap")]
mod tests;
