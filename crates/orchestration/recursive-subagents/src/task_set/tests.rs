use super::TaskSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_aborts_and_joins_every_pending_task() {
    let tasks = TaskSet::new();
    let dropped = Arc::new(AtomicBool::new(false));
    let flag = DropFlag(Arc::clone(&dropped));
    let pending = tasks.spawn(async move {
        let _flag = flag;
        std::future::pending::<()>().await;
    });
    let finished = tasks.spawn(async { 7 });
    assert_eq!(finished.await.unwrap(), 7);
    assert_eq!(tasks.len(), 1);

    assert!(tasks.close(Duration::from_secs(5)).await);

    assert!(tasks.is_closed());
    assert!(tasks.is_empty());
    assert!(
        dropped.load(Ordering::SeqCst),
        "the pending future was dropped"
    );
    assert!(pending.await.unwrap_err().is_cancelled());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_task_spawned_after_close_never_runs() {
    let tasks = TaskSet::new();
    tasks.abort_all();
    let ran = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&ran);
    let late = tasks.spawn(async move {
        observed.store(true, Ordering::SeqCst);
    });

    assert!(late.await.unwrap_err().is_cancelled());
    assert!(!ran.load(Ordering::SeqCst));
    assert!(tasks.close(Duration::from_secs(5)).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clones_name_one_set() {
    let tasks = TaskSet::new();
    let clone = tasks.clone();
    let task = clone.spawn(std::future::pending::<()>());
    assert_eq!(tasks.len(), 1);

    assert!(tasks.close(Duration::from_secs(5)).await);

    assert!(clone.is_closed());
    assert!(task.await.unwrap_err().is_cancelled());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_reports_a_task_that_does_not_yield_within_the_bound() {
    let tasks = TaskSet::new();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let _stuck = tasks.spawn(async move {
        let _ = started.send(());
        // Never yields while blocked, so an abort cannot drop it.
        let _ = released.recv();
    });
    ready.await.unwrap();

    assert!(!tasks.close(Duration::from_millis(50)).await);

    release.send(()).unwrap();
    assert!(tasks.close(Duration::from_secs(5)).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panicking_task_reports_the_panic_and_leaves_the_set() {
    let tasks = TaskSet::new();
    let task = tasks.spawn(async {
        panic!("synthetic task panic");
    });
    assert!(task.await.unwrap_err().is_panic());
    assert!(tasks.is_empty());
    assert!(tasks.close(Duration::from_secs(5)).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_task_starts_through_the_sets_runtime() {
    struct Counting {
        native: plakous::runtime::TokioRuntime,
        spawned: std::sync::atomic::AtomicUsize,
    }
    impl plakous::runtime::Runtime for Counting {
        fn spawn(
            &self,
            task: plakous::runtime::RuntimeTask,
        ) -> Box<dyn plakous::runtime::SpawnedTask> {
            self.spawned.fetch_add(1, Ordering::SeqCst);
            self.native.spawn(task)
        }
        fn sleep(&self, duration: Duration) -> plakous::runtime::RuntimeSleep {
            self.native.sleep(duration)
        }
        fn now(&self) -> Duration {
            self.native.now()
        }
    }
    let runtime = Arc::new(Counting {
        native: plakous::runtime::TokioRuntime::new(),
        spawned: std::sync::atomic::AtomicUsize::new(0),
    });
    let tasks = TaskSet::on(runtime.clone());
    assert_eq!(tasks.spawn(async { 1 }).await.unwrap(), 1);
    let pending = tasks.spawn(std::future::pending::<()>());
    assert_eq!(runtime.spawned.load(Ordering::SeqCst), 2);
    assert!(tasks.close(Duration::from_secs(5)).await);
    assert!(pending.await.unwrap_err().is_cancelled());
}
