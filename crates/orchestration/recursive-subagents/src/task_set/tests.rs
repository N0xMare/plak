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
async fn close_aborts_tasks_spawned_into_a_join_set() {
    let tasks = TaskSet::new();
    let mut set = tokio::task::JoinSet::new();
    tasks.spawn_into(&mut set, std::future::pending::<()>());
    assert_eq!(tasks.len(), 1);

    assert!(tasks.close(Duration::from_secs(5)).await);

    assert!(set.join_next().await.unwrap().unwrap_err().is_cancelled());
}
