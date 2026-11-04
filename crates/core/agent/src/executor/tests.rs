use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use plakous_model::{Runtime, RuntimeBlockingTask, RuntimeSleep, RuntimeTask, SpawnedTask};

use super::{Cancelled, Elapsed, Executor};
use crate::TokioRuntime;

/// The native runtime, counting what goes through it.
#[derive(Default)]
struct Counting {
    native: TokioRuntime,
    spawns: AtomicUsize,
    blocking: AtomicUsize,
    sleeps: AtomicUsize,
}

impl Runtime for Counting {
    fn spawn(&self, task: RuntimeTask) -> Box<dyn SpawnedTask> {
        self.spawns.fetch_add(1, Ordering::Relaxed);
        self.native.spawn(task)
    }

    fn spawn_blocking(&self, work: RuntimeBlockingTask) -> Box<dyn SpawnedTask> {
        self.blocking.fetch_add(1, Ordering::Relaxed);
        self.native.spawn_blocking(work)
    }

    fn sleep(&self, duration: Duration) -> RuntimeSleep {
        self.sleeps.fetch_add(1, Ordering::Relaxed);
        self.native.sleep(duration)
    }

    fn now(&self) -> Duration {
        self.native.now()
    }
}

#[tokio::test(start_paused = true)]
async fn a_timeout_yields_the_output_or_elapses_on_the_runtime_clock() {
    let executor = Executor::new(Arc::new(TokioRuntime::new()));
    assert_eq!(
        executor.timeout(Duration::from_secs(1), async { 7 }).await,
        Ok(7)
    );
    let started = executor.now();
    let pending = std::future::pending::<()>();
    assert_eq!(
        executor.timeout(Duration::from_secs(5), pending).await,
        Err(Elapsed)
    );
    assert_eq!(
        executor.now().saturating_duration_since(started),
        Duration::from_secs(5)
    );
    let deadline = executor.now() + Duration::from_secs(2);
    let late = executor.sleep(Duration::from_secs(3));
    assert_eq!(executor.timeout_at(deadline, late).await, Err(Elapsed));
    assert_eq!(executor.now(), deadline);
}

#[tokio::test(start_paused = true)]
async fn an_interval_ticks_at_once_then_skips_missed_ticks_onto_its_grid() {
    let executor = Executor::new(Arc::new(TokioRuntime::new()));
    let start = executor.now();
    let mut interval = executor.interval(Duration::from_millis(100));
    interval.tick().await;
    assert_eq!(executor.now(), start);
    interval.tick().await;
    assert_eq!(executor.now(), start + Duration::from_millis(100));
    executor.sleep(Duration::from_millis(250)).await;
    interval.tick().await;
    assert_eq!(executor.now(), start + Duration::from_millis(350));
    interval.tick().await;
    assert_eq!(executor.now(), start + Duration::from_millis(400));
}

#[tokio::test]
async fn spawned_work_runs_on_the_runtime_in_scope_and_reports_its_output() {
    let runtime = Arc::new(Counting::default());
    let executor = Executor::new(Arc::clone(&runtime) as Arc<dyn Runtime>);
    let nested = executor.spawn(async {
        // The task runs in the executor's scope: this sleep is the runtime's.
        super::sleep(Duration::from_millis(1)).await;
        3
    });
    assert_eq!(nested.await, Ok(3));
    assert_eq!(executor.spawn_blocking(|| 4).await, Ok(4));
    assert_eq!(runtime.spawns.load(Ordering::Relaxed), 1);
    assert_eq!(runtime.blocking.load(Ordering::Relaxed), 1);
    assert_eq!(runtime.sleeps.load(Ordering::Relaxed), 1);
    let waited = executor.scope(async { super::sleep(Duration::from_millis(1)).await });
    waited.await;
    assert_eq!(runtime.sleeps.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn an_aborted_task_reports_cancelled_once_its_future_is_dropped() {
    let executor = Executor::new(Arc::new(TokioRuntime::new()));
    let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
    let task = executor.spawn(async move {
        std::future::pending::<()>().await;
        drop(sender);
    });
    task.abort();
    assert_eq!(task.await, Err(Cancelled));
    assert!(
        receiver.await.is_err(),
        "the aborted task dropped its future"
    );
}

/// Holds blocking work instead of running it, so a test decides when.
#[derive(Default)]
struct Deferred {
    native: TokioRuntime,
    blocking: std::sync::Mutex<Vec<RuntimeBlockingTask>>,
}

impl Runtime for Deferred {
    fn spawn(&self, task: RuntimeTask) -> Box<dyn SpawnedTask> {
        self.native.spawn(task)
    }

    fn spawn_blocking(&self, work: RuntimeBlockingTask) -> Box<dyn SpawnedTask> {
        self.blocking.lock().unwrap().push(work);
        self.native.spawn(Box::pin(async {}))
    }

    fn sleep(&self, duration: Duration) -> RuntimeSleep {
        self.native.sleep(duration)
    }

    fn now(&self) -> Duration {
        self.native.now()
    }
}

#[tokio::test]
async fn aborted_blocking_work_that_has_not_started_never_runs() {
    let runtime = Arc::new(Deferred::default());
    let executor = Executor::new(Arc::clone(&runtime) as Arc<dyn Runtime>);
    let ran = Arc::new(AtomicUsize::new(0));
    let task = executor.spawn_blocking({
        let ran = Arc::clone(&ran);
        move || ran.fetch_add(1, Ordering::Relaxed)
    });
    task.abort();
    let held = std::mem::take(&mut *runtime.blocking.lock().unwrap());
    for work in held {
        work();
    }
    assert_eq!(task.await, Err(Cancelled));
    assert_eq!(ran.load(Ordering::Relaxed), 0);
}
