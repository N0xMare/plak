use super::*;

#[tokio::test(start_paused = true)]
async fn cancelled_publication_retains_repeated_revivals_for_heartbeat_and_release() -> TestResult {
    for required_revivals in [1, 2] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let store = Arc::new(TestCreateStore::default());
        let sessions = DurableSessionRuntime::current(
            store.clone(),
            RuntimeDurability::Required,
            NonZeroU64::new(3).unwrap(),
        )?;
        let model = provenance_test_model(947_000, false)?;
        let agent = sessions
            .create(
                "create:cancel-revived-publication",
                provenance_test_builder(workspace, &model)?,
            )
            .await?;
        let initial = store.active_lease().ok_or("initial lease")?;
        *lock_mutex(&store.publication_clock) = Some((tokio::time::Instant::now(), 997));
        store.allow_revival.store(true, Ordering::Release);
        store
            .full_after_completed_round
            .store(true, Ordering::Release);
        let turn = agent
            .prompt("retain the response until cancellation")
            .await?;
        store.full_round_refused.notified().await;
        // Small renewal/revival writes fit while the prepared checkpoint does
        // not. Each five-second retry outlives this three-second lease.
        store.full_commits.store(true, Ordering::Release);
        while store.revive_calls.load(Ordering::Acquire) < required_revivals {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let revived = store.active_lease().ok_or("revived lease")?;
        assert_eq!(revived.fence(), initial.fence());
        assert_ne!(revived, initial);
        let control = turn.control();
        let _ = control.request_cancel().await;
        let result = turn.result().await?;
        assert!(matches!(
            result.outcome(),
            TurnResultOutcome::RecoveryRequired(_)
        ));
        // A previous grant may renew once idempotently, but a grant two
        // revivals behind cannot. Neither case may poison terminal authority.
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert!(!agent.inner.authority.lease_lost_for_test());
        assert_eq!(model.calls.load(Ordering::Acquire), 1);
        assert_eq!(control.shared.published_result_count(), 1);
        store.full_commits.store(false, Ordering::Release);
        agent.shutdown().await?;
        let state = lock_mutex(&store.state);
        assert!(state.active_lease.is_none());
        let released = state.released_lease.as_ref().ok_or("released lease")?;
        assert_eq!(released.fence(), revived.fence());
        assert_eq!(released.owner(), revived.owner());
        assert_ne!(
            released
                .expires_at()
                .same_domain_cmp(revived.expires_at())?,
            std::cmp::Ordering::Less
        );
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn unreadable_clock_retries_transient_renewal_without_losing_authority() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(3).unwrap(),
    )?;
    let model = provenance_test_model(948_000, false)?;
    let agent = sessions
        .create(
            "create:unreadable-renewal-clock",
            provenance_test_builder(workspace, &model)?,
        )
        .await?;
    let before = store.renew_calls.load(Ordering::Acquire);
    store.fail_lease_clock.store(true, Ordering::Release);
    store.unavailable_renewal.store(true, Ordering::Release);
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(store.renew_calls.load(Ordering::Acquire) > before + 4);
    assert!(!agent.inner.authority.lease_lost_for_test());
    assert!(!agent.storage_unavailable());
    store.fail_lease_clock.store(false, Ordering::Release);
    store.unavailable_renewal.store(false, Ordering::Release);
    let result = agent
        .prompt("continue after clock recovery")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn store_clock_jump_past_expiry_revives_before_the_next_prompt() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).unwrap(),
    )?;
    let model = provenance_test_model(946_000, false)?;
    let agent = sessions
        .create(
            "create:suspended-owner",
            provenance_test_builder(workspace, &model)?,
        )
        .await?;
    let held = lock_mutex(&store.state)
        .active_lease
        .clone()
        .ok_or("held lease")?;
    store.allow_revival.store(true, Ordering::Release);
    // Only the store clock jumps, as on suspend; Tokio's renewal deadline
    // has not elapsed when the expiry check first observes the jump.
    *lock_mutex(&store.publication_clock) = Some((
        tokio::time::Instant::now(),
        held.expires_at().seconds() + 31,
    ));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(store.revive_calls.load(Ordering::Acquire) > 0);
    assert!(!agent.inner.authority.lease_lost_for_test());
    assert_eq!(model.calls.load(Ordering::Acquire), 0);
    let result = agent
        .prompt("continue after suspend")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    let revived = lock_mutex(&store.state)
        .active_lease
        .clone()
        .ok_or("revived lease")?;
    assert_eq!(revived.fence(), held.fence());
    assert_eq!(revived.owner(), held.owner());
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn foreign_takeover_while_storage_parked_refuses_revival_without_publication() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(3).unwrap(),
    )?;
    let model = provenance_test_model(945_000, false)?;
    let agent = sessions
        .create(
            "create:foreign-during-outage",
            provenance_test_builder(workspace, &model)?,
        )
        .await?;
    store
        .full_after_completed_round
        .store(true, Ordering::Release);
    store.remain_full_after_round.store(true, Ordering::Release);
    store.allow_revival.store(true, Ordering::Release);
    let turn = agent.prompt("retain the one response").await?;
    store.full_round_refused.notified().await;
    let prior = lock_mutex(&store.accepted_commits).len();
    let held = lock_mutex(&store.state)
        .active_lease
        .clone()
        .ok_or("held lease")?;
    *lock_mutex(&store.lease_read_override) = Some(held.expires_at());
    tokio::time::advance(Duration::from_millis(200)).await;
    tokio::task::yield_now().await;
    assert!(
        !agent.inner.authority.lease_lost_for_test(),
        "expiry alone must not end the capacity-parked owner"
    );
    assert_eq!(turn.control().shared.published_result_count(), 0);
    let foreign = store
        .acquire(AcquireBranch::new(
            held.session(),
            RunId::try_from(uuid::Uuid::from_u128(945_999))?,
            held.lease_seconds(),
        ))
        .await?;
    assert_ne!(foreign.fence(), held.fence());
    store.reject_renewal.store(true, Ordering::Release);
    store.full_writes.store(false, Ordering::Release);
    let result = turn.result().await?;
    assert!(matches!(
        result.outcome(),
        TurnResultOutcome::RecoveryRequired(_)
    ));
    assert!(store.revive_calls.load(Ordering::Acquire) > 0);
    assert_eq!(lock_mutex(&store.accepted_commits).len(), prior);
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    assert_eq!(
        lock_mutex(&store.accepted_commits)
            .iter()
            .flat_map(|(_, facts)| facts)
            .filter(|fact| matches!(fact, crate::feed::FeedFact::TurnTerminal { .. }))
            .count(),
        0
    );
    let _ = agent.shutdown().await;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn all_writes_including_renewal_park_beyond_call_deadlines_then_complete_once() -> TestResult
{
    for after_model_output in [false, true] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let store = Arc::new(TestCreateStore::default());
        *lock_mutex(&store.publication_clock) = Some((tokio::time::Instant::now(), 997));
        store.allow_revival.store(true, Ordering::Release);
        let sessions = DurableSessionRuntime::current(
            store.clone(),
            RuntimeDurability::Required,
            NonZeroU64::new(30).unwrap(),
        )?;
        let model = provenance_test_model(943_000, false)?;
        let agent = sessions
            .create(
                "create:all-writes-full",
                provenance_test_builder(workspace, &model)?,
            )
            .await?;
        let held = lock_mutex(&store.state)
            .active_lease
            .clone()
            .ok_or("held lease")?;
        let started = tokio::time::Instant::now();
        if after_model_output {
            store
                .full_after_completed_round
                .store(true, Ordering::Release);
            store.remain_full_after_round.store(true, Ordering::Release);
        } else {
            *lock_mutex(&store.full_until) = Some(started + Duration::from_secs(45));
        }
        let turn = agent.prompt("wait for the authoritative writer").await?;
        let control = turn.control();
        if after_model_output {
            store.full_round_refused.notified().await;
            assert_eq!(model.calls.load(Ordering::Acquire), 1);
            *lock_mutex(&store.full_until) = Some(started + Duration::from_secs(45));
            store.full_writes.store(false, Ordering::Release);
            // The outage still holds: the round was refused and the result is
            // not published until the park ends. The pre-model iteration cannot
            // assert that here; prompt() returns only after the timed park, and
            // the terminal may already be published.
            assert_eq!(control.shared.published_result_count(), 0);
        }
        let result = turn.result().await?;
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{:?}",
            result.outcome()
        );
        assert!(started.elapsed() >= Duration::from_secs(45));
        // A parked checkpoint holds the lease gate and retries revival itself;
        // before admission the outage instead refuses renewal directly.
        if !after_model_output {
            assert!(store.full_renewals.load(Ordering::Acquire) > 0);
        }
        assert!(store.revive_calls.load(Ordering::Acquire) > 0);
        assert!(store.lease_now()?.seconds() > held.expires_at().seconds());
        assert_eq!(control.shared.published_result_count(), 1);
        let revived = lock_mutex(&store.state)
            .active_lease
            .clone()
            .ok_or("revived lease")?;
        assert_eq!(revived.fence(), held.fence());
        assert_eq!(revived.owner(), held.owner());
        assert_eq!(model.calls.load(Ordering::Acquire), 1);
        assert_eq!(
            lock_mutex(&store.accepted_commits)
                .iter()
                .flat_map(|(_, facts)| facts)
                .filter(|fact| matches!(fact, crate::feed::FeedFact::TurnTerminal { .. }))
                .count(),
            1
        );
        agent.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn operator_cancel_exhausts_a_parked_root_without_fabricating_a_terminal() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).unwrap(),
    )?;
    let model = provenance_test_model(944_000, false)?;
    let agent = sessions
        .create(
            "create:cancel-full",
            provenance_test_builder(workspace, &model)?,
        )
        .await?;
    store
        .full_after_completed_round
        .store(true, Ordering::Release);
    store.remain_full_after_round.store(true, Ordering::Release);
    let turn = agent.prompt("retain this response").await?;
    store.full_round_refused.notified().await;
    let control = turn.control();
    assert!(agent.storage_unavailable());
    assert_eq!(control.shared.published_result_count(), 0);
    let _ = control.request_cancel().await;
    let result = turn.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::RecoveryRequired(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    assert_eq!(control.shared.published_result_count(), 1);
    assert_eq!(
        lock_mutex(&store.accepted_commits)
            .iter()
            .flat_map(|(_, facts)| facts)
            .filter(|fact| matches!(fact, crate::feed::FeedFact::TurnTerminal { .. }))
            .count(),
        0
    );
    store.full_writes.store(false, Ordering::Release);
    let _ = agent.shutdown().await;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn storage_full_during_parked_session_acquire_waits_without_replaying_work() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).unwrap(),
    )?;
    let model = provenance_test_model(942_000, false)?;
    let agent = runtime
        .create(
            "create:parked-storage",
            provenance_test_builder(workspace.clone(), &model)?,
        )
        .await?;
    let first = agent.prompt("first turn").await?.result().await?;
    assert!(matches!(first.outcome(), TurnResultOutcome::Completed(_)));
    agent.shutdown().await?;
    let address = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    store.full_next_acquire.store(true, Ordering::Release);
    let started = tokio::time::Instant::now();
    let DurableRuntimeOpen::Opened(reopened) = runtime
        .open(
            "open:parked-storage",
            address,
            provenance_test_builder(workspace, &model)?,
        )
        .await?
    else {
        return Err("parked session did not reopen".into());
    };
    assert!(started.elapsed() >= Duration::from_secs(5));
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        1,
        "waking the owner does not resend the first turn"
    );
    let result = reopened.prompt("next turn").await?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    reopened.shutdown().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn storage_full_retains_an_in_flight_tool_result_without_reexecuting_the_tool() -> TestResult
{
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).unwrap(),
    )?;
    let model = provenance_test_model(941_000, true)?;
    let agent = sessions
        .create(
            "create:tool-storage-outage",
            provenance_test_builder(workspace, &model)?,
        )
        .await?;
    store.full_after_tool_result.store(true, Ordering::Release);
    let turn = agent.prompt("use the probe").await?;
    store.full_round_refused.notified().await;
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    let result = turn.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(result.tool_calls(), 1);
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    assert_eq!(store.recover_commit_calls.load(Ordering::Acquire), 0);
    assert_eq!(committed_tool_result_texts(result.checkpoint()).len(), 1);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn storage_full_after_model_output_retries_publication_without_paid_redispatch() -> TestResult
{
    for lease_seconds in [30, 3] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let store = Arc::new(TestCreateStore::default());
        if lease_seconds == 3 {
            *lock_mutex(&store.publication_clock) = Some((tokio::time::Instant::now(), 997));
            store.allow_revival.store(true, Ordering::Release);
        }
        let sessions = DurableSessionRuntime::current(
            store.clone(),
            RuntimeDurability::Required,
            NonZeroU64::new(lease_seconds).unwrap(),
        )?;
        let model = provenance_test_model(940_000, false)?;
        let agent = sessions
            .create(
                "create:storage-outage",
                provenance_test_builder(workspace, &model)?,
            )
            .await?;
        store
            .full_after_completed_round
            .store(true, Ordering::Release);
        let command_id = CommandId::new("root:storage-outage")?;
        let turn = agent
            .command(TurnCommand::Start {
                command_id: command_id.clone(),
                input: "retain the response".into(),
            })
            .await?
            .into_turn()
            .ok_or("admitted root turn")?;
        store.full_round_refused.notified().await;
        store.lose_commit_ack_on.store(
            store.commit_calls.load(Ordering::Acquire) + 1,
            Ordering::Release,
        );
        let requests = model.calls.load(Ordering::Acquire);
        assert_eq!(requests, 1);
        // The publication retry lands the already received model result. The turn
        // remains owned while parked; no reopen or terminal is manufactured.
        let result = turn.result().await?;
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{:?}",
            result.outcome()
        );
        assert_eq!(model.calls.load(Ordering::Acquire), requests);
        assert_eq!(store.recover_commit_calls.load(Ordering::Acquire), 1);
        let terminal_count = lock_mutex(&store.accepted_commits)
            .iter()
            .flat_map(|(_, facts)| facts)
            .filter(|fact| matches!(fact, crate::feed::FeedFact::TurnTerminal { .. }))
            .count();
        assert_eq!(terminal_count, 1);
        let duplicate = agent
            .command(TurnCommand::Start {
                command_id,
                input: "retain the response".into(),
            })
            .await?;
        assert!(duplicate.acknowledgement().is_accepted());
        assert!(
            duplicate.turn().is_none(),
            "a repeated accept receipt never creates another turn"
        );
        assert_eq!(model.calls.load(Ordering::Acquire), requests);
        agent.shutdown().await?;
    }
    Ok(())
}
