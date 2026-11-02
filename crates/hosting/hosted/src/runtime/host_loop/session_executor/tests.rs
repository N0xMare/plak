use super::SessionExecutor;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[test]
fn dropping_the_executor_closes_and_joins_its_session_tasks() {
    let executor = SessionExecutor::new().unwrap();
    let tasks = executor.tasks().clone();
    let dropped = Arc::new(AtomicBool::new(false));
    let flag = DropFlag(Arc::clone(&dropped));
    executor
        .block_on(async {
            tasks.spawn(async move {
                let _flag = flag;
                std::future::pending::<()>().await;
            });
        })
        .unwrap();
    assert_eq!(tasks.len(), 1);

    drop(executor);

    assert!(tasks.is_closed(), "the session closed its own set");
    assert!(tasks.is_empty(), "every session task was joined");
    assert!(dropped.load(Ordering::SeqCst));
}
