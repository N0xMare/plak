//! The native [`Runtime`] on Tokio.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use plakous_model::{Runtime, RuntimeBlockingTask, RuntimeSleep, RuntimeTask, SpawnedTask};
use tokio::time::Instant;

/// [`Runtime`] on the Tokio runtime the caller is running on. Its clock is
/// Tokio's, so a paused test clock pauses [`Runtime::now`] too.
///
/// [`Runtime::spawn`] panics when called outside a Tokio runtime, as
/// `tokio::spawn` does. [`Runtime::spawn_blocking`] runs on Tokio's blocking
/// pool.
#[derive(Clone, Copy, Debug)]
pub struct TokioRuntime {
    origin: Instant,
}

impl TokioRuntime {
    /// A runtime whose [`Runtime::now`] counts from this call.
    #[must_use]
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }

    /// The process's shared native runtime.
    #[must_use]
    pub fn shared() -> Arc<dyn Runtime> {
        static SHARED: OnceLock<Arc<dyn Runtime>> = OnceLock::new();
        Arc::clone(SHARED.get_or_init(|| Arc::new(Self::new())))
    }
}

impl Default for TokioRuntime {
    fn default() -> Self {
        Self::new()
    }
}

struct TokioTask(tokio::task::AbortHandle);

impl SpawnedTask for TokioTask {
    fn abort(&self) {
        self.0.abort();
    }

    fn is_finished(&self) -> bool {
        self.0.is_finished()
    }
}

impl Runtime for TokioRuntime {
    fn spawn(&self, task: RuntimeTask) -> Box<dyn SpawnedTask> {
        Box::new(TokioTask(tokio::spawn(task).abort_handle()))
    }

    fn spawn_blocking(&self, work: RuntimeBlockingTask) -> Box<dyn SpawnedTask> {
        Box::new(TokioTask(tokio::task::spawn_blocking(work).abort_handle()))
    }

    fn sleep(&self, duration: Duration) -> RuntimeSleep {
        Box::pin(tokio::time::sleep(duration))
    }

    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// The agent's default runtime: the Tokio runtime current when the agent was
/// built, held by handle so a host thread outside it can still start work on
/// it. Every instance counts [`Runtime::now`] from one process origin, so
/// instants from two of them compare.
pub(crate) struct BoundTokioRuntime {
    handle: tokio::runtime::Handle,
}

impl BoundTokioRuntime {
    /// The runtime the caller is running on, or `None` outside Tokio.
    pub(crate) fn current() -> Option<Self> {
        let handle = tokio::runtime::Handle::try_current().ok()?;
        process_origin();
        Some(Self { handle })
    }
}

/// One origin for every bound runtime, set far in the past so that no
/// runtime's clock reads earlier than it: a paused test clock created before
/// the origin was first taken still advances from a positive reading.
fn process_origin() -> Instant {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    *ORIGIN.get_or_init(|| {
        let now = Instant::now();
        [10 * 365, 365, 1]
            .into_iter()
            .find_map(|days| now.checked_sub(Duration::from_secs(days * 24 * 60 * 60)))
            .unwrap_or(now)
    })
}

impl Runtime for BoundTokioRuntime {
    fn spawn(&self, task: RuntimeTask) -> Box<dyn SpawnedTask> {
        Box::new(TokioTask(self.handle.spawn(task).abort_handle()))
    }

    fn spawn_blocking(&self, work: RuntimeBlockingTask) -> Box<dyn SpawnedTask> {
        Box::new(TokioTask(self.handle.spawn_blocking(work).abort_handle()))
    }

    fn sleep(&self, duration: Duration) -> RuntimeSleep {
        // A sleep binds to the timer of the runtime it is created in: the
        // awaiting runtime, as `tokio::time::sleep` does, and the build
        // runtime only for a caller outside every Tokio runtime.
        if tokio::runtime::Handle::try_current().is_ok() {
            return Box::pin(tokio::time::sleep(duration));
        }
        let _entered = self.handle.enter();
        Box::pin(tokio::time::sleep(duration))
    }

    fn now(&self) -> Duration {
        process_origin().elapsed()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests may assert with unwrap")]
mod tests {
    use std::time::Duration;

    use plakous_model::Runtime;

    use super::BoundTokioRuntime;

    fn runtime(paused: bool) -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(paused)
            .build()
            .unwrap()
    }

    /// A paused clock whose base predates the process origin's first reading
    /// still advances on the bound runtime's monotonic clock.
    #[test]
    fn a_paused_clock_older_than_the_first_origin_reading_still_advances() {
        let paused = runtime(true);
        std::thread::sleep(Duration::from_millis(50));
        let other = runtime(false);
        other.block_on(async { BoundTokioRuntime::current().unwrap().now() });
        paused.block_on(async {
            let bound = BoundTokioRuntime::current().unwrap();
            let before = bound.now();
            assert!(before > Duration::ZERO);
            tokio::time::advance(Duration::from_millis(100)).await;
            assert!(bound.now() >= before + Duration::from_millis(100));
        });
    }

    /// A sleep made in another runtime binds to that runtime's timer, so a
    /// paused awaiting runtime fires it by auto-advance.
    #[test]
    fn a_sleep_binds_to_the_awaiting_runtime() {
        let build = runtime(false);
        let bound = build.block_on(async { BoundTokioRuntime::current().unwrap() });
        let paused = runtime(true);
        paused.block_on(async {
            let started = tokio::time::Instant::now();
            bound.sleep(Duration::from_secs(3600)).await;
            assert!(started.elapsed() >= Duration::from_secs(3600));
        });
    }
}
