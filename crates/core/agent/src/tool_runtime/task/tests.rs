use super::*;

#[tokio::test]
async fn only_first_joiner_takes_the_executor_value_without_copying() -> Result<(), ToolRuntimeError>
{
    use plakous_tools::{ExecutionValue, ProcessValue};
    let output = "executor payload".repeat(100);
    let pointer = output.as_ptr();
    let executor: ToolTaskFuture = Box::pin(async move {
        ToolOutcome::success_text("same text")?.with_value(ExecutionValue::Process(ProcessValue {
            job_id: 1,
            omitted_bytes: 0,
            status: "exited".into(),
            exit_code: Some(0),
            session_id: None,
            output,
            truncated: false,
            full_output: None,
            program: None,
        }))
    });
    let task = Arc::new(TokioTask::new());
    task.complete(executor.await);
    let mut first = task_join(Arc::clone(&task)).await?;
    let Some(ExecutionValue::Process(value)) = first.take_value() else {
        panic!("first joiner lost the value");
    };
    assert_eq!(value.output.as_ptr(), pointer);
    assert!(
        lock(&task.state)
            .result
            .as_mut()
            .expect("stored result")
            .as_mut()
            .expect("success")
            .take_value()
            .is_none()
    );
    let mut second = task_join(task).await?;
    assert!(second.take_value().is_none());
    assert_eq!(first.parts(), second.parts());
    Ok(())
}

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
async fn abort_requested_while_installation_waits_is_not_lost() {
    let task = Arc::new(TokioTask::new());
    let worker = tokio::spawn(std::future::pending::<()>());
    let abort = worker.abort_handle();
    let state = lock(&task.state);
    let installer = std::thread::spawn({
        let task = Arc::clone(&task);
        move || task.install_abort(&abort)
    });
    // Give the installer time to reach the state lock, then publish the request the way
    // `request_abort` does while it owns the lock and still sees no handle.
    std::thread::sleep(std::time::Duration::from_millis(50));
    task.abort_requested.store(true, Ordering::Release);
    drop(state);
    assert!(installer.join().is_ok());
    let joined = tokio::time::timeout(std::time::Duration::from_secs(10), worker).await;
    let Ok(Err(error)) = joined else {
        panic!("the requested abort must reach the installed worker");
    };
    assert!(error.is_cancelled());
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
async fn concurrent_join_and_drain_waiters_all_observe_completion() -> Result<(), ToolRuntimeError>
{
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
async fn shutdown_closes_admission_aborts_and_is_clone_idempotent() -> Result<(), ToolRuntimeError>
{
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
