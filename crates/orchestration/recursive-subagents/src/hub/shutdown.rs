use super::{
    Agent, Arc, ChildStartup, HOST_SHUTDOWN_DETACH_TIMEOUT, HOST_SHUTDOWN_RELEASE_TIMEOUT,
    ProductTopologyRuntime, RetirementFaultSource, TopologyShutdownError, lookup, process_hub,
    record_retirement_fault,
};

/// Drops a root session lease. The last lease drains live children and removes the hub.
pub async fn release_root_lease(tree_id: &str) -> Result<(), TopologyShutdownError> {
    let runtime =
        process_hub()
            .release_root_lease(tree_id)
            .map_err(|error| TopologyShutdownError {
                tree_failure: Some(error.to_string()),
                child_failures: Vec::new(),
            })?;
    if let Some(runtime) = runtime {
        shutdown_runtime(runtime).await?;
    }
    Ok(())
}

/// Closes admission and aborts recovery before collecting runtime capabilities.
/// Unlike hub retirement, this neither settles providers nor saves tree rows.
pub async fn abandon_live_agents(tree_id: &str) -> Vec<Agent> {
    let Some(runtime) = lookup(tree_id) else {
        return Vec::new();
    };
    runtime.close_for_shutdown();
    let pass = runtime.recovery_pass.lock().ok().and_then(|mut slot| {
        slot.running = false;
        slot.rerun = false;
        slot.task.take()
    });
    if let Some(pass) = pass {
        pass.abort();
        let _ = pass.await;
    }
    for task in runtime.take_explicit_recovers() {
        task.abort();
        let _ = task.await;
    }
    let live = runtime.live.lock().await;
    for startup in live.starting.values() {
        startup.request_cancel();
    }
    for retirement in live.retiring.values() {
        retirement.abort.abort();
    }
    live.children
        .values()
        .filter_map(|child| child.agent())
        .chain(
            live.retiring
                .values()
                .filter_map(|retirement| retirement.agent.clone()),
        )
        .collect()
}

async fn startup_results_owned(startups: Vec<(String, ChildStartup)>) -> Vec<Result<(), String>> {
    futures_util::future::join_all(startups.iter().map(|(_, startup)| async move {
        startup
            .join_cancel()
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }))
    .await
}

pub(super) async fn shutdown_runtime(
    runtime: Arc<ProductTopologyRuntime>,
) -> Result<(), TopologyShutdownError> {
    let mut failure = close_runtime_admission(&runtime).await;
    let tasks = runtime.driver.tasks();
    join_runtime_children(runtime, &mut failure).await;
    // The tree's tasks end with the tree, never with one root's release: what
    // the bounded joins above left running is aborted and joined here.
    if !tasks.close(HOST_SHUTDOWN_DETACH_TIMEOUT).await {
        tracing::warn!(target: "plak.topology", remaining = tasks.len(),
            "tree tasks did not stop within their join bound");
    }
    if failure.tree_failure.is_none() && failure.child_failures.is_empty() {
        Ok(())
    } else {
        Err(failure)
    }
}

async fn close_runtime_admission(runtime: &ProductTopologyRuntime) -> TopologyShutdownError {
    runtime.close_for_shutdown();
    // A recovery pass reopens child sessions; it ends (or is aborted) before
    // the drain, so no reopen outlives the leases this shutdown releases.
    let pass = runtime
        .recovery_pass
        .lock()
        .ok()
        .and_then(|mut slot| slot.task.take());
    if let Some(mut pass) = pass
        && tokio::time::timeout(HOST_SHUTDOWN_RELEASE_TIMEOUT, &mut pass)
            .await
            .is_err()
    {
        pass.abort();
        let _ = tokio::time::timeout(HOST_SHUTDOWN_DETACH_TIMEOUT, pass).await;
    }
    finish_explicit_recovers(runtime).await;
    let mut failure = TopologyShutdownError::default();
    if let Ok(mut tree) = runtime.driver().lock() {
        tree.emit_shutdown_detaches(runtime.observer().as_ref());
        // Detaches are observations, not rows: nothing to publish unless the
        // tree moved since the store last acknowledged it.
        if tree.revision != tree.stored_revision
            && let Err(error) = runtime.driver.save_guarded(&mut tree)
        {
            tracing::warn!(
                target: "plak.topology",
                tree_id = %tree.tree_id,
                error = %error.message,
                "tree was not persisted at hub shutdown"
            );
            failure.tree_failure = Some(error.message);
        }
    } else {
        failure.tree_failure = Some("topology tree lock is poisoned".into());
    }
    failure
}

async fn finish_explicit_recovers(runtime: &ProductTopologyRuntime) {
    let tasks = runtime.take_explicit_recovers();
    if tasks.is_empty() {
        return;
    }
    let deadline = tokio::time::Instant::now() + HOST_SHUTDOWN_RELEASE_TIMEOUT;
    let mut aborted = Vec::new();
    for mut task in tasks {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if tokio::time::timeout(left, &mut task).await.is_err() {
            task.abort();
            aborted.push(task);
        }
    }
    for task in aborted {
        let _ = tokio::time::timeout(HOST_SHUTDOWN_DETACH_TIMEOUT, task).await;
    }
}

async fn join_runtime_children(
    runtime: Arc<ProductTopologyRuntime>,
    failure: &mut TopologyShutdownError,
) {
    let startups = {
        let live = runtime.live.lock().await;
        live.starting
            .iter()
            .map(|(id, startup)| {
                startup.request_cancel();
                (id.clone(), startup.clone())
            })
            .collect::<Vec<_>>()
    };
    let startup_task = runtime
        .driver
        .tasks()
        .spawn(startup_results_owned(startups.clone()));
    let startup_abort = startup_task.abort_handle();
    let retirements = {
        let mut live = runtime.live.lock().await;
        let ids: Vec<_> = live.children.keys().cloned().collect();
        for id in ids {
            live.start_retire(
                runtime.live.clone(),
                runtime.driver.clone(),
                &id,
                None,
                true,
            );
        }
        live.retiring
            .iter()
            .map(|(id, retirement)| (id.clone(), retirement.clone()))
            .collect::<Vec<_>>()
    };
    let completions = || {
        futures_util::future::join_all(
            retirements
                .iter()
                .map(|(_, retirement)| retirement.completion.clone()),
        )
    };
    // Shutdown bounds the join, not owned release. Only explicit process-exit
    // abandonment may abort a retirement while it still owns runtime authority.
    let results = if let Ok(results) = tokio::time::timeout(HOST_SHUTDOWN_RELEASE_TIMEOUT, async {
        let results = completions().await;
        if let Err(error) = runtime.driver.join_retirement_tasks().await {
            failure.tree_failure.get_or_insert(error);
        }
        results
    })
    .await
    {
        results
    } else {
        failure.tree_failure.get_or_insert_with(|| {
            "retirement completion exceeded the shutdown bound; owner retained".to_owned()
        });
        retirements
            .iter()
            .map(|_| Err("child retirement exceeded the shutdown bound; owner retained".to_owned()))
            .collect()
    };
    for ((agent_id, retirement), result) in retirements.into_iter().zip(results) {
        if let Err(error) = result {
            record_retirement_fault(
                &runtime.driver,
                &agent_id,
                retirement.expected_attempt.as_ref(),
                RetirementFaultSource::Joiner,
                &error,
            );
            failure.child_failures.push((agent_id, error));
        }
    }
    let startup_results = match tokio::time::timeout(HOST_SHUTDOWN_DETACH_TIMEOUT, startup_task)
        .await
    {
        Ok(Ok(results)) => results,
        Ok(Err(_)) => {
            vec![Err("child startup settlement task failed".into()); startups.len()]
        }
        Err(_) => {
            startup_abort.abort();
            vec![Err("child startup settlement exceeded the shutdown bound".into()); startups.len()]
        }
    };
    for ((id, startup), result) in startups.into_iter().zip(startup_results) {
        if let Err(error) = result {
            record_retirement_fault(
                &runtime.driver,
                &id,
                startup.expected_attempt.as_ref(),
                RetirementFaultSource::Joiner,
                &error,
            );
            failure.child_failures.push((id, error));
        }
    }
}
