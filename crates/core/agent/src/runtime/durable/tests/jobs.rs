use super::*;

#[derive(Default)]
struct RetainedJobHost(Mutex<Option<plakous_tools::JoinedProcessSessions>>);

impl plakous_tools::JobHost for RetainedJobHost {
    fn service(&self, _: SessionId, _: BranchId) -> Option<plakous_tools::JoinedProcessSessions> {
        lock_mutex(&self.0).clone()
    }
    fn attach(
        &self,
        _: plakous_tools::JobOwner,
        fresh: plakous_tools::JoinedProcessSessions,
    ) -> Result<plakous_tools::JoinedProcessSessions, plakous_tools::ExecutionBackendError> {
        let mut held = lock_mutex(&self.0);
        let service = held.get_or_insert(fresh);
        service.stop_foreground()?;
        Ok(service.clone())
    }
}

#[tokio::test]
async fn terminal_release_stops_hosted_jobs_before_releasing_lease() -> TestResult {
    terminal_release_hosted_job(false).await
}

#[tokio::test]
async fn phantom_terminal_snapshot_is_hidden_from_agent_jobs() -> TestResult {
    terminal_release_hosted_job(true).await
}

async fn terminal_release_hosted_job(phantom: bool) -> TestResult {
    let directory = tempfile::tempdir()?;
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            96_800,
            false,
        )
        .with_tool_first(),
    );
    let (registry, tool) = session_update_registry(SessionUpdateFixtureMode::Promoted)?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:terminal-hosted-job",
            session_update_builder(
                Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
                model,
                registry,
            )?
            .job_host(Arc::new(RetainedJobHost::default())),
        )
        .await?;
    agent.prompt("start a job").await?.result().await?;
    let service = lock_mutex(&tool.sessions).clone().ok_or("service")?;
    assert_eq!(service.jobs()[0].status, plakous_tools::JobStatus::Running);
    if phantom {
        let mut foreign = service.jobs()[0].clone();
        foreign.number += 100;
        foreign.status = plakous_tools::JobStatus::Exited(Some(0));
        plakous_tools::internal::restore_job_terminals(&service, &[foreign.clone()]);
        assert!(!agent.jobs().iter().any(|job| job.number == foreign.number));
    }
    agent.shutdown().await?;
    assert_eq!(
        service.jobs()[0].status,
        plakous_tools::JobStatus::Stopped(plakous_tools::JobStopReason::SessionExit)
    );
    assert_eq!(
        agent.checkpoint().manifest().jobs().rows()[0]
            .terminal
            .as_ref()
            .ok_or("terminal")?
            .stopper,
        Some(plakous_tools::JobStopReason::SessionExit)
    );
    assert!(lock_mutex(&store.state).active_lease.is_none());
    Ok(())
}

#[tokio::test]
async fn explicit_transfer_keeps_hosted_job_and_number_on_same_branch() -> TestResult {
    transfer_hosted_job(false).await
}

#[tokio::test]
async fn recovery_required_keeps_hosted_job_until_host_selects_transfer() -> TestResult {
    transfer_hosted_job(true).await
}

async fn transfer_hosted_job(panic: bool) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            97_800,
            false,
        )
        .with_tool_first(),
    );
    let (registry, tool) = session_update_registry(SessionUpdateFixtureMode::Promoted)?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(1).ok_or("lease")?,
    )?;
    let host = Arc::new(RetainedJobHost::default());
    let agent = sessions
        .create(
            "create:transfer-job",
            session_update_builder(workspace.clone(), model.clone(), registry.clone())?
                .job_host(host.clone()),
        )
        .await?;
    agent.prompt("start a job").await?.result().await?;
    let before = agent.jobs()[0].clone();
    let service = lock_mutex(&tool.sessions).clone().ok_or("service")?;
    if panic {
        store.fail_quiescent_terminal();
        let result = agent.prompt("force recovery").await?.result().await?;
        assert!(matches!(
            result.outcome(),
            TurnResultOutcome::RecoveryRequired(_)
        ));
        store.reject_renewal.store(true, Ordering::Release);
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            crate::runtime::terminal::wait_for_completion(agent.inner.completion.clone()),
        )
        .await?;
        assert_eq!(service.jobs()[0].status, plakous_tools::JobStatus::Running);
    }
    let _ = crate::runtime::shutdown_for_transfer(&agent).await;
    store.reject_renewal.store(false, Ordering::Release);
    assert_eq!(service.jobs()[0].status, plakous_tools::JobStatus::Running);
    assert!(lock_mutex(&store.state).active_lease.is_none());
    let address =
        DurableSessionAddress::new(agent.session_id(), agent.branch_id(), agent.checkpoint_id());
    drop(agent);
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open(
            "open:transfer-job",
            address,
            session_update_builder(workspace, model, registry)?.job_host(host),
        )
        .await?
    else {
        return Err("transfer reopen refused".into());
    };
    assert_eq!(reopened.jobs()[0].number, before.number);
    assert_eq!(reopened.jobs()[0].status, plakous_tools::JobStatus::Running);
    reopened.shutdown().await?;
    assert_eq!(
        service.jobs()[0].status,
        plakous_tools::JobStatus::Stopped(plakous_tools::JobStopReason::SessionExit)
    );
    Ok(())
}

#[tokio::test]
async fn jobs_carry_crash_then_recovery_release_preserves_rows_as_unknown() -> TestResult {
    crash_then_recovery_release_preserves_jobs(false).await
}

#[tokio::test]
async fn jobs_carry_abandonment_of_working_turn_preserves_rows_and_counter() -> TestResult {
    crash_then_recovery_release_preserves_jobs(true).await
}

async fn crash_then_recovery_release_preserves_jobs(working: bool) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            94_800,
            false,
        )
        .with_tool_first()
        .with_block_call_after(2),
    );
    let (registry, tool) = session_update_registry(SessionUpdateFixtureMode::Promoted)?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:crash-job",
            session_update_builder(workspace.clone(), model.clone(), registry.clone())?
                .job_counter_seed(11)?,
        )
        .await?;
    agent.prompt("start").await?.result().await?;
    let _unfinished = if working {
        let turn = agent.prompt("leave the next turn Working").await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while model.calls.load(Ordering::Acquire) < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        Some(turn)
    } else {
        None
    };
    let prior = agent.checkpoint();
    let service = lock_mutex(&tool.sessions).clone().ok_or("service")?;
    agent.begin_abandon();
    agent.abort_driver_for_test();
    let lease = lock_mutex(&store.state)
        .active_lease
        .clone()
        .ok_or("lease")?;
    // A recovery release has no process-terminal evidence.
    store.release(lease).await?;
    let address = DurableSessionAddress::new(agent.session_id(), agent.branch_id(), prior.id());
    drop(agent);
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open(
            "open:crash-job",
            address,
            session_update_builder(workspace, model, registry)?,
        )
        .await?
    else {
        return Err("crash reopen refused".into());
    };
    assert_eq!(reopened.abandoned_turn().is_some(), working);
    assert_eq!(
        reopened.checkpoint().manifest().jobs(),
        prior.manifest().jobs()
    );
    assert_eq!(
        reopened.jobs()[0].status,
        plakous_tools::JobStatus::OutcomeUnknown
    );
    assert_eq!(reopened.checkpoint().manifest().jobs().next_id(), 12);
    service.stop_jobs(
        plakous_tools::JobStopReason::User,
        Duration::from_millis(200),
    )?;
    reopened.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn fast_abandon_stops_jobs_and_records_terminal_before_releasing_lease() -> TestResult {
    abandon_hosted_job(false, false).await
}

#[tokio::test]
async fn abandon_record_failure_still_releases_exact_lease() -> TestResult {
    abandon_hosted_job(true, false).await
}

#[tokio::test]
async fn abandon_stop_failure_still_releases_exact_lease() -> TestResult {
    abandon_hosted_job(false, true).await
}

async fn abandon_hosted_job(fail_record: bool, fail_stop: bool) -> TestResult {
    let directory = tempfile::tempdir()?;
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            93_800,
            false,
        )
        .with_tool_first(),
    );
    let (registry, tool) = session_update_registry(SessionUpdateFixtureMode::Promoted)?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(
            "create:exit-job",
            session_update_builder(
                Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
                model,
                registry,
            )?,
        )
        .await?;
    agent.prompt("start a job").await?.result().await?;
    let process = lock_mutex(&tool.sessions)
        .clone()
        .ok_or("fixture service")?;
    assert_eq!(process.jobs()[0].status, plakous_tools::JobStatus::Running);
    let before = agent.checkpoint();
    if fail_stop {
        let (_sender, receiver) = tokio::sync::watch::channel(Some(Err(AgentError::new(
            AgentErrorKind::Tool,
            "injected job stop failure",
        ))));
        *lock_mutex(&agent.inner.job_stop) = crate::runtime::JobStopState::Stopping(receiver);
        agent.begin_abandon();
        assert!(agent.abandon_and_release().await.is_err());
        assert!(lock_mutex(&store.state).active_lease.is_none());
        return Ok(());
    }
    agent.begin_abandon();
    if fail_record {
        *lock_mutex(&store.reject_next_commit) = Some(SessionStoreError::BackendUnavailable);
        assert!(agent.abandon_and_release().await.is_err());
        assert!(lock_mutex(&store.state).active_lease.is_none());
        return Ok(());
    }
    agent.abandon_and_release().await?;
    assert_eq!(
        process.jobs()[0].status,
        plakous_tools::JobStatus::Stopped(plakous_tools::JobStopReason::SessionExit),
    );
    let after = agent.checkpoint();
    assert_eq!(before.manifest().canonical(), after.manifest().canonical());
    assert_eq!(
        after.manifest().jobs().rows()[0]
            .terminal
            .as_ref()
            .ok_or("exit evidence")?
            .stopper,
        Some(plakous_tools::JobStopReason::SessionExit),
    );
    assert!(lock_mutex(&store.state).active_lease.is_none());
    Ok(())
}

#[tokio::test]
async fn process_notice_admission_is_atomic_and_exactly_once() -> TestResult {
    let directory = tempfile::tempdir()?;
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            92_800,
            false,
        )
        .with_tool_first(),
    );
    let (registry, tool) = session_update_registry(SessionUpdateFixtureMode::Promoted)?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:notice",
            session_update_builder(
                Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
                model,
                registry,
            )?,
        )
        .await?;
    agent.prompt("start a job").await?.result().await?;
    let process = lock_mutex(&tool.sessions).clone().ok_or("service")?;
    let id = process.session_for_number(1).ok_or("handle")?;
    process.stop(id, plakous_tools::JobStopReason::User)?;
    let result = agent.prompt("next intent").await?.result().await?;
    assert_eq!(result.checkpoint().manifest().jobs().rows(), []);
    assert_eq!(agent.jobs(), [] as [plakous_tools::JobView; 0]);
    let history = result.checkpoint().manifest().canonical().history();
    let segments = history.segments();
    let notice_index = segments
        .iter()
        .position(|segment| {
            segment.items().iter().any(|item| {
                item.blocks().iter().any(|block| {
                    matches!(block, ContentBlock::Text(text) if text.text().contains("<process_notice"))
                })
            })
        })
        .ok_or("notice missing")?;
    let notice = &segments[notice_index];
    assert_eq!(notice.kind(), HistorySegmentKind::Steering);
    assert_eq!(notice.items()[0].actor(), Actor::User);
    assert_eq!(notice.items()[0].provenance(), Provenance::Instruction);
    assert_eq!(
        segments[notice_index + 1].kind(),
        HistorySegmentKind::RecentIntent
    );
    {
        let state = lock_mutex(&store.state);
        let admission = state
            .stored_commits
            .values()
            .find(|stored| {
                let segments = stored
                    .checkpoint()
                    .manifest()
                    .canonical()
                    .history()
                    .segments();
                segments.len() == notice_index + 2
            })
            .ok_or("notice and prompt did not commit together")?;
        assert_eq!(admission.checkpoint().manifest().jobs().rows(), []);
    }
    let later = agent.prompt("third intent").await?.result().await?;
    assert_eq!(
        later
            .checkpoint()
            .manifest()
            .canonical()
            .history()
            .segments()
            .iter()
            .filter(|segment| segment.kind() == HistorySegmentKind::Steering)
            .count(),
        1
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn promoted_job_survives_turn_and_records_quiescent_terminal_once() -> TestResult {
    let directory = tempfile::tempdir()?;
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            92_800,
            false,
        )
        .with_tool_first(),
    );
    let (registry, tool) = session_update_registry(SessionUpdateFixtureMode::Promoted)?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(
            "create:promoted-job",
            session_update_builder(
                Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
                Arc::clone(&model),
                registry.clone(),
            )?
            .job_counter_seed(7)?,
        )
        .await?;
    let result = agent.prompt("start a job").await?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    let checkpoint = result.checkpoint();
    let ledger = checkpoint.manifest().jobs();
    assert_eq!(ledger.next_id(), 8);
    assert_eq!(ledger.rows().len(), 1);
    assert_eq!(ledger.rows()[0].number, 7);
    assert!(ledger.rows()[0].terminal.is_none());
    assert_eq!(agent.jobs()[0].status, plakous_tools::JobStatus::Running);
    assert!(!agent.record_job_terminals().await?);

    // The next turn's ordinary commits must preserve the first turn's row.
    let second = agent.prompt("continue").await?.result().await?;
    assert!(matches!(second.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(second.checkpoint().manifest().jobs(), ledger);
    let process = lock_mutex(&tool.sessions)
        .clone()
        .ok_or("fixture service")?;
    let id = process.session_for_number(7).ok_or("job handle")?;
    process.write_stdin(
        id,
        b"finish\n",
        plakous_tools::JoinedProcessInputDisposition::KeepOpen,
    )?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while process.joined_terminal_status(id).is_none() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await?;
    let before = agent.checkpoint();
    let mut events = agent.attach(ViewRequest::metadata()).await?.into_parts().1;
    assert!(agent.record_job_terminals().await?);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let event = events.recv().await?;
            if matches!(event.kind(), AgentEventKind::JobFinished { number: 7 }) {
                break;
            }
        }
        Ok::<_, crate::SubscriptionEnd>(())
    })
    .await??;
    let after = agent.checkpoint();
    assert_ne!(before.id(), after.id());
    assert_eq!(before.manifest().canonical(), after.manifest().canonical());
    let terminal = after.manifest().jobs().rows()[0]
        .terminal
        .as_ref()
        .ok_or("terminal evidence")?;
    assert_eq!(terminal.status, crate::manifest::JobTerminalStatus::Exited);
    assert_eq!(terminal.exit_code, Some(7));
    assert_eq!(terminal.tail, "terminal\u{fffd}tail");
    assert!(!agent.record_job_terminals().await?);
    let jobs = after.manifest().jobs().clone();
    let topology = after.manifest().topology();
    let (_, compaction_model) = recovery_compaction_fixture(
        Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    let configured = configured_test_model(
        compaction_model,
        descriptor.clone(),
        artifact_authority(&descriptor, 1)?,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let compactor = AgentBuilder::new(
        configured,
        registry.clone(),
        Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
    )
    .request_policy(AgentRequestPolicy::new(
        GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
        ReasoningPolicy::Disabled,
    ))
    .identity_sources(
        Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
        Arc::new(SystemIdGenerator),
    );
    let compacted = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            "compact:pending-job".to_owned(),
            topology.session_id,
            topology.branch_id,
            topology.checkpoint_id,
            AgentCompactionReason::Manual,
            compactor,
        ))
        .await
        .map_err(|error| format!("job-carry compaction: {error:?}"))?;
    assert!(matches!(
        compacted,
        AgentCompactSessionOutcome::Compacted { .. }
    ));
    assert_eq!(agent.checkpoint().manifest().jobs(), &jobs);
    // Every stored checkpoint after promotion carries the row, including
    // operation-only and terminal publications in the later turn.
    {
        let state = lock_mutex(&store.state);
        for stored in state.stored_commits.values() {
            let manifest = stored.checkpoint().manifest();
            if manifest.topology().branch_revision
                >= checkpoint.manifest().topology().branch_revision
            {
                assert_eq!(manifest.jobs().rows().len(), 1);
                assert_eq!(manifest.jobs().next_id(), 8);
            }
        }
    }
    agent.shutdown().await?;
    let address = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    drop(agent);
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open(
            "open:terminal-job",
            address,
            session_update_builder(
                Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
                model,
                registry,
            )?,
        )
        .await?
    else {
        return Err("terminal job prevented quiescent reopen".into());
    };
    let snapshots = &reopened.inner.job_sessions;
    assert_eq!(
        reopened.jobs()[0].session_id,
        jobs.rows()[0].session_id.as_bytes()
    );
    let id = snapshots
        .session_for_number(7)
        .ok_or("terminal snapshot was not hydrated")?;
    let poll = snapshots.poll_from(
        id,
        plakous_tools::JoinedProcessOutputCursor::new(0),
        4096,
        256,
    )?;
    assert_eq!(
        poll.status(),
        plakous_tools::JoinedProcessStatus::Exited(Some(7))
    );
    assert_eq!(
        poll.records()[0].bytes().as_ref(),
        "terminal\u{fffd}tail".as_bytes()
    );
    assert_eq!(
        snapshots.stop(id, plakous_tools::JobStopReason::Model)?,
        poll.status()
    );
    assert_eq!(reopened.jobs()[0].output, "terminal\u{fffd}tail");
    let reported = reopened
        .prompt("poll completed job")
        .await?
        .result()
        .await?;
    assert!(matches!(
        reported.outcome(),
        TurnResultOutcome::Completed(_)
    ));
    assert_eq!(reported.checkpoint().manifest().jobs().rows(), []);
    assert_eq!(reported.checkpoint().manifest().jobs().next_id(), 8);
    assert_eq!(snapshots.jobs(), [] as [plakous_tools::JobView; 0]);
    assert!(snapshots.session_for_number(7).is_none());
    assert!(!reopened.record_job_terminals().await?);
    reopened.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn review_crash_reopen_running_job_is_pollable_and_consumed() -> TestResult {
    let directory = tempfile::tempdir()?;
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            92_800,
            false,
        )
        .with_tool_first(),
    );
    let (registry, tool) = session_update_registry(SessionUpdateFixtureMode::Promoted)?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(
            "create:promoted-job",
            session_update_builder(
                Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
                Arc::clone(&model),
                registry.clone(),
            )?
            .job_counter_seed(7)?,
        )
        .await?;
    let result = agent.prompt("start a job").await?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    let checkpoint = result.checkpoint();
    let ledger = checkpoint.manifest().jobs();
    assert_eq!(ledger.next_id(), 8);
    assert_eq!(ledger.rows().len(), 1);
    assert_eq!(ledger.rows()[0].number, 7);
    assert!(ledger.rows()[0].terminal.is_none());
    assert_eq!(agent.jobs()[0].status, plakous_tools::JobStatus::Running);
    assert!(!agent.record_job_terminals().await?);

    let address =
        DurableSessionAddress::new(agent.session_id(), agent.branch_id(), checkpoint.id());
    // Expiring the lease models a lost owner without a release checkpoint.
    store.expire_active_lease();
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open(
            "open:lost-job",
            address,
            session_update_builder(
                Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
                Arc::clone(&model),
                registry,
            )?,
        )
        .await?
    else {
        return Err("running job prevented reopen".into());
    };
    let snapshots = &reopened.inner.job_sessions;
    let id = snapshots
        .session_for_number(7)
        .ok_or("lost job snapshot missing")?;
    let poll = snapshots.poll_from(
        id,
        plakous_tools::JoinedProcessOutputCursor::new(0),
        4096,
        256,
    )?;
    assert_eq!(
        poll.status(),
        plakous_tools::JoinedProcessStatus::OutcomeUnknown
    );
    assert_eq!(agent.jobs()[0].status, plakous_tools::JobStatus::Running);
    assert_eq!(
        snapshots.jobs()[0].status,
        plakous_tools::JobStatus::OutcomeUnknown
    );
    let result = reopened.prompt("poll lost job").await?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(tool.dispatches.load(Ordering::Acquire), 1);
    assert!(result.conversation().items().iter().flat_map(ConversationItem::blocks)
        .any(|block| matches!(block, ContentBlock::Text(text) if text.text().contains("Plak stopped unexpectedly while this command was running."))));
    assert_eq!(result.checkpoint().manifest().jobs().rows(), []);
    assert_eq!(snapshots.jobs(), [] as [plakous_tools::JobView; 0]);
    reopened.shutdown().await?;
    let _ = agent.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn review_nul_tail_does_not_block_later_turn_commits() -> TestResult {
    let directory = tempfile::tempdir()?;
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            92_800,
            false,
        )
        .with_tool_first()
        .with_scripted_tool_calls([(3, "probe", "{}".to_owned())]),
    );
    let (registry, tool) = session_update_registry(SessionUpdateFixtureMode::Promoted)?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(
            "create:promoted-job",
            session_update_builder(
                Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
                Arc::clone(&model),
                registry.clone(),
            )?
            .job_counter_seed(7)?,
        )
        .await?;
    let result = agent.prompt("start a job").await?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    let checkpoint = result.checkpoint();
    let ledger = checkpoint.manifest().jobs();
    assert_eq!(ledger.next_id(), 8);
    assert_eq!(ledger.rows().len(), 1);
    assert_eq!(ledger.rows()[0].number, 7);
    assert!(ledger.rows()[0].terminal.is_none());
    assert_eq!(agent.jobs()[0].status, plakous_tools::JobStatus::Running);
    assert!(!agent.record_job_terminals().await?);

    let process = lock_mutex(&tool.sessions)
        .clone()
        .ok_or("fixture service")?;
    let id = process.session_for_number(7).ok_or("job handle")?;
    process.write_stdin(
        id,
        b"finish\n",
        plakous_tools::JoinedProcessInputDisposition::KeepOpen,
    )?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while process.joined_terminal_status(id).is_none() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await?;
    let result = agent
        .prompt("commit after control output")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(result.checkpoint().manifest().jobs().rows(), []);
    assert!(result.conversation().items().iter().flat_map(ConversationItem::blocks)
        .any(|block| matches!(block, ContentBlock::Text(text) if text.text().contains("terminal\u{fffd}tail"))));
    assert!(!agent.record_job_terminals().await?);
    agent.shutdown().await?;
    Ok(())
}
