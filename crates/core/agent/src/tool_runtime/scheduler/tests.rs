use futures::task::noop_waker;

use super::*;

fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(&noop_waker()))
}

fn resource(key: &str) -> Result<ToolConcurrency, ToolRuntimeError> {
    Ok(ToolConcurrency::ResourceExclusive(vec![
        ToolResourceKey::new(key).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::InvalidDefinition,
                "test resource key is invalid",
                &error,
            )
        })?,
    ]))
}

#[test]
fn admission_is_atomic_and_disjoint_work_bypasses_a_blocked_resource()
-> Result<(), Box<dyn std::error::Error>> {
    let scheduler = ToolScheduler::with_waiter_limit(2, 8)?;
    let first_lane = scheduler.runtime_lane(ToolRuntimeConfig::new(2)?);
    let second_lane = scheduler.runtime_lane(ToolRuntimeConfig::new(1)?);
    let held = futures::executor::block_on(SchedulerPermits::acquire(
        &scheduler,
        &first_lane,
        &resource("resource:held")?,
    ))?;
    let blocked_policy = resource("resource:held")?;
    let mut blocked = Box::pin(SchedulerPermits::acquire(
        &scheduler,
        &first_lane,
        &blocked_policy,
    ));
    assert!(poll_once(blocked.as_mut()).is_pending());
    assert_eq!(scheduler.status().active(), 1);

    let disjoint = futures::executor::block_on(SchedulerPermits::acquire(
        &scheduler,
        &second_lane,
        &ToolConcurrency::Parallel,
    ))?;
    assert_eq!(scheduler.status().active(), 2);
    drop(disjoint);
    drop(blocked);
    drop(held);
    Ok(())
}

#[test]
fn cancellation_removes_a_queued_request_and_wakes_the_chain()
-> Result<(), Box<dyn std::error::Error>> {
    let scheduler = ToolScheduler::with_waiter_limit(1, 4)?;
    let lane = scheduler.runtime_lane(ToolRuntimeConfig::new(1)?);
    let held = futures::executor::block_on(SchedulerPermits::acquire(
        &scheduler,
        &lane,
        &ToolConcurrency::Parallel,
    ))?;
    let mut first = Box::pin(SchedulerPermits::acquire(
        &scheduler,
        &lane,
        &ToolConcurrency::Parallel,
    ));
    let mut second = Box::pin(SchedulerPermits::acquire(
        &scheduler,
        &lane,
        &ToolConcurrency::Parallel,
    ));
    assert!(poll_once(first.as_mut()).is_pending());
    assert!(poll_once(second.as_mut()).is_pending());
    drop(first);
    assert_eq!(scheduler.status().waiting(), 1);
    drop(held);
    assert!(matches!(poll_once(second.as_mut()), Poll::Ready(Ok(_))));
    Ok(())
}

#[test]
fn waiter_capacity_and_resource_state_are_bounded() -> Result<(), Box<dyn std::error::Error>> {
    let scheduler = ToolScheduler::with_waiter_limit(1, 1)?;
    let lane = scheduler.runtime_lane(ToolRuntimeConfig::with_waiter_limit(1, 1)?);
    for index in 0..128 {
        let policy = resource(&format!("resource:{index}"))?;
        let permit =
            futures::executor::block_on(SchedulerPermits::acquire(&scheduler, &lane, &policy))?;
        assert_eq!(scheduler.status().active_resource_keys(), 1);
        drop(permit);
        assert_eq!(scheduler.status().active_resource_keys(), 0);
    }
    let held = futures::executor::block_on(SchedulerPermits::acquire(
        &scheduler,
        &lane,
        &ToolConcurrency::Parallel,
    ))?;
    let mut waiting = Box::pin(SchedulerPermits::acquire(
        &scheduler,
        &lane,
        &ToolConcurrency::Parallel,
    ));
    let mut overflow = Box::pin(SchedulerPermits::acquire(
        &scheduler,
        &lane,
        &ToolConcurrency::Parallel,
    ));
    assert!(poll_once(waiting.as_mut()).is_pending());
    assert!(matches!(
        poll_once(overflow.as_mut()),
        Poll::Ready(Err(error)) if error.kind() == ToolRuntimeErrorKind::ResourceLimit
    ));
    drop(waiting);
    drop(held);
    assert_eq!(scheduler.status().active_runtime_lanes(), 0);
    Ok(())
}

#[test]
fn default_scheduler_is_process_shared_but_explicit_instances_are_isolated()
-> Result<(), Box<dyn std::error::Error>> {
    let first = default_scheduler();
    let second = default_scheduler();
    assert!(first.shares_budget_with(&second));
    assert!(!first.shares_budget_with(&ToolScheduler::new(64)?));
    Ok(())
}
