use super::*;

#[derive(Default)]
struct GroupSteerTool {
    dispatches: AtomicUsize,
    release: AtomicBool,
}

impl plakous_tools::ToolExecutor for GroupSteerTool {
    fn execute(
        self: Arc<Self>,
        _invocation: ToolInvocation,
        context: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        Box::pin(async move {
            self.dispatches.fetch_add(1, Ordering::AcqRel);
            wait_for_scheduler_fact(&self.release).await;
            assert!(!context.cancellation().is_cancelled());
            ToolOutcome::success_text("group ran")
        })
    }
}

#[tokio::test]
async fn steer_closes_or_runs_the_entire_read_only_group() -> TestResult {
    for before_dispatch in [true, false] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let descriptor = durable_test_descriptor("test.route")?;
        let model = Arc::new(
            ArtifactModel::new(
                descriptor.clone(),
                artifact_authority(&descriptor, 1)?,
                0,
                94_300,
                false,
            )
            .with_tool_first()
            .with_second_direct_tool()?,
        );
        let provider_release = Arc::new(Notify::new());
        if before_dispatch {
            *lock_mutex(&model.held_call_release) = Some(provider_release.clone());
            model.held_call.store(0, Ordering::Release);
        }
        let tool = Arc::new(GroupSteerTool::default());
        let mut registry = ToolRegistryBuilder::default();
        registry.register(ToolRegistration::new(
            RegisteredToolId::new("steer-group.probe")?,
            ToolExecutorId::new("steer-group.executor")?,
            plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"steer-group"])?,
            ToolDefinition::new(
                ToolName::new("probe")?,
                "1.0.0",
                "Read-only group probe.",
                ToolInputContract::structured(
                    json!({"$schema":"https://json-schema.org/draft/2020-12/schema", "type":"object", "properties": {}, "required": [], "additionalProperties": false}),
                )?,
                ToolOutputContract::model_content(128, 1_048_576)?,
                ToolConcurrency::Parallel,
                ToolEffect::ReadOnly,
            )?,
            tool.clone(),
        ))?;
        let sessions = DurableSessionRuntime::current(
            Arc::new(TestCreateStore::default()),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("nonzero lease")?,
        )?;
        let agent = sessions
            .create(
                "create:steer-group",
                session_update_builder(workspace, model.clone(), registry.freeze())?,
            )
            .await?;
        let turn = agent.prompt("read group").await?;
        if before_dispatch {
            tokio::time::timeout(Duration::from_secs(2), model.held_call_started.notified())
                .await?;
        } else {
            tokio::time::timeout(
                Duration::from_secs(2),
                wait_for_scheduler_count(&tool.dispatches, 1),
            )
            .await?;
        }
        turn.control().steer("new instruction").await?;
        provider_release.notify_one();
        tool.release.store(true, Ordering::Release);
        let result = turn.result().await?;
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{:?}",
            result.outcome()
        );
        assert_eq!(
            tool.dispatches.load(Ordering::Acquire),
            if before_dispatch { 0 } else { 2 }
        );
        let results = result
            .conversation()
            .items()
            .iter()
            .flat_map(ConversationItem::blocks)
            .filter_map(|block| match block {
                ContentBlock::ToolResult(result) => Some(result),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 2);
        let expected = if before_dispatch {
            "not run: superseded by your message"
        } else {
            "group ran"
        };
        assert!(results.iter().all(|result| {
            result
                .parts()
                .iter()
                .any(|part| matches!(part, ToolResultPart::Text(text) if text.text() == expected))
        }));
        agent.shutdown().await?;
    }
    Ok(())
}

#[test]
fn steer_neither_promotes_a_nested_process_nor_preempts_its_empty_poll() -> TestResult {
    // The whole scenario and each nested-process wait are liveness bounds.
    // A cold loaded lane has exceeded the 10s scenario budget and the 3s
    // observation waits; neither number is the behavior under test. The
    // 100ms check below stays: it asserts that a steer does not shorten
    // the observation.
    run_cancel_cell_scenario_with_timeout(
        RUNTIME_EXECUTOR_STACK_BYTES,
        Duration::from_secs(45),
        || async {
            for empty_poll in [false, true] {
                let directory = tempfile::tempdir()?;
                let evidence = Arc::new(CancelCellEvidence::default());
                evidence.steer_fixture.store(true, Ordering::Release);
                evidence.terminal_poll.store(empty_poll, Ordering::Release);
                evidence.empty_poll.store(empty_poll, Ordering::Release);
                let agent = cancel_cell_agent(
                    CancelCellMode::ProcessYield,
                    evidence.clone(),
                    directory.path(),
                )
                .await?;
                let mut turn = agent.prompt("open process cell").await?;
                let mut events = turn.take_events()?;
                let expected = if empty_poll {
                    "process_session_write"
                } else {
                    PROCESS_SESSION_CAPABILITY_NAME
                };
                tokio::time::timeout(Duration::from_secs(15), async {
                    loop {
                        let event = events.recv().await?;
                        if matches!(event.kind(), AgentEventKind::ToolStarted { identity, .. }
                        if identity.canonical_name().as_str() == expected)
                        {
                            break;
                        }
                    }
                    Ok::<_, Box<dyn std::error::Error>>(())
                })
                .await??;
                turn.control().steer("new instruction").await?;
                let premature = tokio::time::timeout(
                    Duration::from_millis(100),
                    wait_for_scheduler_count(&evidence.wait_count, 1),
                )
                .await;
                assert!(premature.is_err(), "steer shortened a nested observation");
                std::fs::write(directory.path().join("ready"), b"ready")?;
                tokio::time::timeout(
                    Duration::from_secs(15),
                    wait_for_scheduler_count(&evidence.wait_count, 1),
                )
                .await?;
                assert_eq!(evidence.counters.terminates.load(Ordering::Acquire), 0);
                if !empty_poll {
                    assert!(
                        agent.checkpoint().manifest().jobs().rows().is_empty(),
                        "steer promoted a nested process"
                    );
                }
                let identity = if empty_poll { "TERMINAL" } else { "SLEEP" };
                assert_eq!(
                    lock_mutex(&evidence.deliveries)
                        .iter()
                        .filter(|(id, ok, _)| id == identity && *ok)
                        .count(),
                    1
                );
                let delivered = lock_mutex(&evidence.deliveries)
                    .iter()
                    .find(|(id, ok, _)| id == identity && *ok)
                    .ok_or("nested result absent")?
                    .2
                    .clone();
                let process_result: serde_json::Value = serde_json::from_str(&delivered)?;
                assert_eq!(
                    process_result["exit_code"],
                    json!(7),
                    "steer returned a nonterminal nested observation"
                );
                evidence.release_cell.store(true, Ordering::Release);
                let result = turn.result().await?;
                assert!(
                    matches!(result.outcome(), TurnResultOutcome::Completed(_)),
                    "{:?}",
                    result.outcome()
                );
                agent.shutdown().await?;
            }
            Ok(())
        },
    )
}

#[test]
fn steer_yields_initial_cell_response_and_outer_wait_without_joining() -> TestResult {
    run_cancel_cell_scenario(|| async {
        for (python, mode) in [
            (true, CancelCellMode::InitialResponsePending),
            (true, CancelCellMode::WaitWindow),
            (false, CancelCellMode::InitialResponsePending),
            (false, CancelCellMode::WaitWindow),
        ] {
            let directory = tempfile::tempdir()?;
            let evidence = Arc::new(CancelCellEvidence::default());
            evidence.python_fixture.store(python, Ordering::Release);
            evidence.steer_fixture.store(true, Ordering::Release);
            let agent = cancel_cell_agent(mode, Arc::clone(&evidence), directory.path()).await?;
            let turn = agent.prompt("open cell").await?;
            wait_for_scheduler_fact(if mode == CancelCellMode::WaitWindow {
                &evidence.wait_started
            } else {
                &evidence.initial_response_started
            })
            .await;
            turn.control().steer("new instruction").await?;
            let expected_waits = if mode == CancelCellMode::WaitWindow {
                2
            } else {
                1
            };
            let yielded = tokio::time::timeout(
                Duration::from_secs(2),
                wait_for_scheduler_count(&evidence.wait_count, expected_waits),
            )
            .await;
            if yielded.is_err() {
                turn.control().cancel().await?;
                let _ = agent.shutdown().await;
            }
            assert!(yielded.is_ok(), "steer did not yield a computing cell");
            assert_eq!(evidence.counters.terminates.load(Ordering::Acquire), 0);
            assert_eq!(
                evidence.counters.session_shutdowns.load(Ordering::Acquire),
                0
            );
            evidence.release_cell.store(true, Ordering::Release);
            let result = turn.result().await?;
            assert!(
                matches!(result.outcome(), TurnResultOutcome::Completed(_)),
                "{:?}",
                result.outcome()
            );
            assert!(result.conversation().items().iter().flat_map(ConversationItem::blocks).any(|block|
                matches!(block, ContentBlock::ToolResult(tool) if tool.parts().iter().any(|part|
                    matches!(part, ToolResultPart::Text(text) if text.text().contains("cell settled"))))));
            agent.shutdown().await?;
        }
        Ok(())
    })
}

#[test]
fn steer_waits_for_nested_mutation_settlement_then_yields_exactly_once() -> TestResult {
    run_cancel_cell_scenario(|| async {
        let directory = tempfile::tempdir()?;
        let evidence = Arc::new(CancelCellEvidence::default());
        evidence.steer_fixture.store(true, Ordering::Release);
        let agent = cancel_cell_agent(
            CancelCellMode::ExecInFlight,
            Arc::clone(&evidence),
            directory.path(),
        )
        .await?;
        let turn = agent.prompt("open mutating cell").await?;
        wait_for_scheduler_fact(&evidence.block_started).await;
        turn.control().steer("new instruction").await?;
        assert_eq!(evidence.wait_count.load(Ordering::Acquire), 0);
        assert!(!evidence.block_joined.load(Ordering::Acquire));
        evidence.release_block.store(true, Ordering::Release);
        let yielded = tokio::time::timeout(
            Duration::from_secs(2),
            wait_for_scheduler_count(&evidence.wait_count, 1),
        )
        .await;
        if yielded.is_err() {
            turn.control().cancel().await?;
            let _ = agent.shutdown().await;
        }
        assert!(
            yielded.is_ok(),
            "steer did not yield after nested mutation settled"
        );
        assert!(evidence.block_joined.load(Ordering::Acquire));
        assert_eq!(evidence.counters.terminates.load(Ordering::Acquire), 0);
        assert_eq!(
            lock_mutex(&evidence.deliveries)
                .iter()
                .filter(|(id, ok, _)| id == "BLOCK" && *ok)
                .count(),
            1
        );
        let checkpoint = agent.checkpoint();
        assert_eq!(
            checkpoint
                .manifest()
                .ledger()
                .operations()
                .iter()
                .filter(|record| matches!(
                    record.kind(),
                    DurableOperationKind::CompositionChild { .. }
                ))
                .count(),
            2
        );
        code_cell_abandonment_fold(&checkpoint)?;
        evidence.release_cell.store(true, Ordering::Release);
        let result = turn.result().await?;
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{:?}",
            result.outcome()
        );
        assert_eq!(
            lock_mutex(&evidence.deliveries)
                .iter()
                .filter(|(id, ok, _)| id == "BLOCK" && *ok)
                .count(),
            1
        );
        agent.shutdown().await?;
        Ok(())
    })
}

#[test]
fn steer_drains_running_nested_read_only_execution_before_yield() -> TestResult {
    run_cancel_cell_scenario(|| async {
        let directory = tempfile::tempdir()?;
        let evidence = Arc::new(CancelCellEvidence::default());
        evidence.steer_fixture.store(true, Ordering::Release);
        evidence.read_only_fixture.store(true, Ordering::Release);
        let agent = cancel_cell_agent(
            CancelCellMode::ExecInFlight,
            evidence.clone(),
            directory.path(),
        )
        .await?;
        let turn = agent.prompt("open read-only cell").await?;
        wait_for_scheduler_fact(&evidence.block_started).await;
        turn.control().steer("new instruction").await?;
        assert_eq!(evidence.wait_count.load(Ordering::Acquire), 0);
        evidence.release_block.store(true, Ordering::Release);
        tokio::time::timeout(
            Duration::from_secs(2),
            wait_for_scheduler_count(&evidence.wait_count, 1),
        )
        .await?;
        assert!(evidence.block_joined.load(Ordering::Acquire));
        assert_eq!(evidence.counters.terminates.load(Ordering::Acquire), 0);
        assert_eq!(
            lock_mutex(&evidence.deliveries)
                .iter()
                .filter(|(id, ok, _)| id == "BLOCK" && *ok)
                .count(),
            1
        );
        evidence.release_cell.store(true, Ordering::Release);
        let result = turn.result().await?;
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{:?}",
            result.outcome()
        );
        agent.shutdown().await?;
        Ok(())
    })
}

#[test]
fn steer_yield_after_nested_settlement_wins_over_an_elapsed_outer_deadline() -> TestResult {
    run_cancel_cell_scenario_with_timeout(8 << 20, Duration::from_secs(600), || async {
        let directory = tempfile::tempdir()?;
        let evidence = Arc::new(CancelCellEvidence::default());
        evidence.steer_fixture.store(true, Ordering::Release);
        let (builder, _) = cancel_cell_builder(
            CancelCellMode::ExecInFlight,
            evidence.clone(),
            directory.path(),
        )?;
        let sessions = DurableSessionRuntime::current(
            Arc::new(TestCreateStore::default()),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("nonzero lease")?,
        )?;
        let agent = sessions
            .create(
                "create:steer-deadline",
                builder.config(
                    AgentConfig::default().with_model_operation_timeout(Duration::from_secs(30)),
                ),
            )
            .await?;
        let turn = agent.prompt("open mutating cell").await?;
        wait_for_scheduler_fact(&evidence.block_started).await;
        turn.control().steer("new instruction").await?;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(40)).await;
        evidence.release_block.store(true, Ordering::Release);
        tokio::time::resume();
        let yielded = tokio::time::timeout(
            Duration::from_secs(2),
            wait_for_scheduler_count(&evidence.wait_count, 1),
        )
        .await;
        if yielded.is_err() {
            let _ = agent.shutdown().await;
        }
        assert!(
            yielded.is_ok(),
            "an elapsed outer deadline turned a steer yield into joined failure"
        );
        assert_eq!(evidence.counters.terminates.load(Ordering::Acquire), 0);
        evidence.release_cell.store(true, Ordering::Release);
        let result = turn.result().await?;
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{:?}",
            result.outcome()
        );
        agent.shutdown().await?;
        Ok(())
    })
}

#[test]
fn crash_reopen_after_steer_yield_preserves_settled_mutation_without_replay() -> TestResult {
    run_cancel_cell_scenario(|| async {
        let directory = tempfile::tempdir()?;
        let evidence = Arc::new(CancelCellEvidence::default());
        evidence.steer_fixture.store(true, Ordering::Release);
        let store = Arc::new(TestCreateStore::default());
        let sessions = DurableSessionRuntime::current(
            store.clone(),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("nonzero lease")?,
        )?;
        let (builder, _) = cancel_cell_builder(
            CancelCellMode::ExecInFlight,
            evidence.clone(),
            directory.path(),
        )?;
        let agent = sessions.create("create:steer-crash", builder).await?;
        let turn = agent.prompt("open mutating cell").await?;
        wait_for_scheduler_fact(&evidence.block_started).await;
        turn.control().steer("new instruction").await?;
        evidence.release_block.store(true, Ordering::Release);
        tokio::time::timeout(
            Duration::from_secs(2),
            wait_for_scheduler_count(&evidence.wait_count, 1),
        )
        .await?;
        let source = DurableSessionAddress::new(
            agent.session_id(),
            agent.branch_id(),
            agent.checkpoint().id(),
        );
        // Keep the exact durable prefix, but not the process-local interpreter
        // or driver. Expire its lease just as a fresh process would wait for it.
        let crashed = Arc::new(TestCreateStore::default());
        *lock_mutex(&crashed.state) = lock_mutex(&store.state).clone();
        *lock_mutex(&crashed.genesis) = lock_mutex(&store.genesis).clone();
        *lock_mutex(&crashed.feed_records) = lock_mutex(&store.feed_records).clone();
        crashed.feed_retained_from.store(
            store.feed_retained_from.load(Ordering::Acquire),
            Ordering::Release,
        );
        lock_mutex(&crashed.state).active_lease = None;
        turn.control().cancel().await?;
        let _ = turn.result().await?;
        agent.shutdown().await?;
        let reopening = DurableSessionRuntime::current(
            crashed.clone(),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("nonzero lease")?,
        )?;
        let (builder, model) = cancel_cell_builder(
            CancelCellMode::ExecInFlight,
            evidence.clone(),
            directory.path(),
        )?;
        let target = match reopening.open("open:steer-crash", source, builder).await? {
            DurableRuntimeOpen::Opened(agent) => agent,
            DurableRuntimeOpen::RecoveryRequired { .. } => {
                return Err("exact reopen did not close the interrupted turn".into());
            }
            DurableRuntimeOpen::RestoredWorking { .. } => {
                return Err("vanished cell was restored".into());
            }
        };
        assert!(matches!(
            target.checkpoint().manifest().lifecycle(),
            RuntimeLifecycle::Quiescent
        ));
        assert_eq!(model.calls.load(Ordering::Acquire), 0);
        let mutation_results = target.checkpoint().manifest().ledger().operations().iter()
            .filter(|record| matches!(record.kind(), DurableOperationKind::Effect { .. })
                && record.transitions().iter().any(|transition|
                    matches!(transition.state(), DurableOperationState::Completed(evidence)
                        if String::from_utf8_lossy(evidence.bytes()).contains("nested mutation settled once"))))
            .count();
        assert_eq!(mutation_results, 1);
        assert_eq!(
            lock_mutex(&evidence.deliveries)
                .iter()
                .filter(|(id, ok, _)| id == "BLOCK" && *ok)
                .count(),
            1
        );
        target.shutdown().await?;
        Ok(())
    })
}

#[test]
fn live_cell_does_not_hide_top_level_process_preempt() -> TestResult {
    run_cancel_cell_scenario(|| async {
        let directory = tempfile::tempdir()?;
        let evidence = Arc::new(CancelCellEvidence::default());
        evidence.mixed_tool_fixture.store(true, Ordering::Release);
        let process = Arc::new(SessionUpdateTool::new(SessionUpdateFixtureMode::Promoted));
        process.wait_for_preempt.store(true, Ordering::Release);
        *lock_mutex(&evidence.top_level_process) = Some(process.clone());
        let agent =
            cancel_cell_agent(CancelCellMode::WaitWindow, evidence, directory.path()).await?;
        let turn = agent.prompt("cell and top-level process").await?;
        let started = tokio::time::timeout(Duration::from_secs(3), async {
            while lock_mutex(&process.sessions).is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        if started.is_err() {
            let _ = turn.control().cancel().await;
            let result = turn.result().await?;
            agent.shutdown().await?;
            return Err(format!(
                "process never started: {:?} {}",
                result.outcome(),
                describe_ledger(&result)
            )
            .into());
        }
        turn.control().steer("new instruction").await?;
        let control = turn.control();
        let result = tokio::time::timeout(Duration::from_secs(3), turn.result()).await;
        if result.is_err() {
            control.cancel().await?;
            let _ = agent.shutdown().await;
        }
        assert!(
            result.is_ok(),
            "lineage marker suppressed top-level preempt"
        );
        let result = result??;
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{:?}",
            result.outcome()
        );
        assert_eq!(result.checkpoint().manifest().jobs().rows().len(), 1);
        agent.shutdown().await?;
        Ok(())
    })
}

struct HoldThirdAuthorization {
    revision: ToolPolicyRevision,
    calls: AtomicUsize,
    blocked: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}

impl ToolPolicy for HoldThirdAuthorization {
    fn revision(&self) -> ToolPolicyRevision {
        self.revision.clone()
    }
    fn evaluate(&self, _request: ToolPolicyRequest) -> ToolPolicyFuture {
        let hold = self.calls.fetch_add(1, Ordering::AcqRel) == 2;
        let blocked = self.blocked.clone();
        let release = self.release.clone();
        Box::pin(async move {
            if hold {
                blocked.store(true, Ordering::Release);
                wait_for_scheduler_fact(&release).await;
            }
            Ok(ToolPolicyDecision::Allowed)
        })
    }
}

#[test]
fn live_cell_top_level_read_only_group_is_superseded_before_dispatch() -> TestResult {
    run_cancel_cell_scenario(|| async {
        let directory = tempfile::tempdir()?;
        let evidence = Arc::new(CancelCellEvidence::default());
        evidence.mixed_tool_fixture.store(true, Ordering::Release);
        evidence.read_only_fixture.store(true, Ordering::Release);
        evidence.release_block.store(true, Ordering::Release);
        let (builder, model) = cancel_cell_builder(
            CancelCellMode::WaitWindow,
            evidence.clone(),
            directory.path(),
        )?;
        let _ = model;
        let blocked = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let builder = builder.tool_policy(Arc::new(HoldThirdAuthorization {
            revision: ToolPolicyRevision::new("test.hold-third", 1)?,
            calls: AtomicUsize::new(0),
            blocked: blocked.clone(),
            release: release.clone(),
        }));
        let sessions = DurableSessionRuntime::current(
            Arc::new(TestCreateStore::default()),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("lease")?,
        )?;
        let agent = sessions.create("create:mixed-group", builder).await?;
        let turn = agent.prompt("cell and read-only group").await?;
        if tokio::time::timeout(Duration::from_secs(3), wait_for_scheduler_fact(&blocked))
            .await
            .is_err()
        {
            let _ = turn.control().cancel().await;
            let result = turn.result().await?;
            agent.shutdown().await?;
            return Err(format!(
                "policy never blocked: {:?} {}",
                result.outcome(),
                describe_ledger(&result)
            )
            .into());
        }
        turn.control().steer("supersede group").await?;
        release.store(true, Ordering::Release);
        let result = turn.result().await?;
        assert!(
            !evidence.block_started.load(Ordering::Acquire),
            "top-level group dispatched after steer"
        );
        let effects = result
            .checkpoint()
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .filter(|op| matches!(op.kind(), DurableOperationKind::Effect { .. }))
            .collect::<Vec<_>>();
        assert_eq!(effects.len(), 3);
        for (index, op) in effects[..2].iter().enumerate() {
            assert!(
                matches!(op.kind(), DurableOperationKind::Effect { source_index, .. } if *source_index as usize == index)
            );
            assert!(
                op.transitions()
                    .iter()
                    .any(|t| matches!(t.state(), DurableOperationState::ProvenNotDispatched(_)))
            );
            assert!(
                !op.transitions()
                    .iter()
                    .any(|t| matches!(t.state(), DurableOperationState::Dispatched))
            );
        }
        assert!(
            effects[2]
                .transitions()
                .iter()
                .any(|t| matches!(t.state(), DurableOperationState::Dispatched))
        );

        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{:?}",
            result.outcome()
        );
        agent.shutdown().await?;
        Ok(())
    })
}

#[test]
fn ten_minute_cell_wait_does_not_fence_at_model_operation_deadline() -> TestResult {
    run_cancel_cell_scenario_with_timeout(8 << 20, Duration::from_secs(620), || async {
        let directory = tempfile::tempdir()?;
        let evidence = Arc::new(CancelCellEvidence::default());
        evidence.long_wait_fixture.store(true, Ordering::Release);
        let agent = cancel_cell_agent(
            CancelCellMode::WaitWindow,
            evidence.clone(),
            directory.path(),
        )
        .await?;
        let turn = agent.prompt("wait ten minutes").await?;
        wait_for_scheduler_fact(&evidence.wait_started).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(301)).await;
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            evidence.counters.terminates.load(Ordering::Acquire),
            0,
            "wait fenced at the model deadline"
        );
        tokio::time::advance(Duration::from_secs(299)).await;
        evidence.release_cell.store(true, Ordering::Release);
        let result = turn.result().await?;
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{:?}",
            result.outcome()
        );
        assert_eq!(evidence.counters.terminates.load(Ordering::Acquire), 0);
        agent.shutdown().await?;
        Ok(())
    })
}

#[test]
fn execute_deadline_suspends_during_nested_execution_and_rearms() -> TestResult {
    assert_nested_observation_deadline(CancelCellMode::ExecInFlight, 40, 20, true, false)
}

#[test]
fn wait_deadline_suspends_during_nested_execution_and_rearms() -> TestResult {
    assert_nested_observation_deadline(CancelCellMode::WaitWindow, 40, 4, true, false)
}

#[test]
fn execute_deadline_rearms_after_mutating_nested_execution() -> TestResult {
    assert_nested_observation_deadline(CancelCellMode::ExecInFlight, 40, 20, false, false)
}

#[test]
fn wait_deadline_rearms_after_mutating_nested_execution() -> TestResult {
    assert_nested_observation_deadline(CancelCellMode::WaitWindow, 40, 4, false, false)
}

#[test]
fn short_mutating_nested_execution_keeps_observation_window() -> TestResult {
    assert_nested_observation_deadline(CancelCellMode::ExecInFlight, 1, 1, false, false)
}

#[test]
fn settled_nested_execution_then_stalled_cell_hits_rearmed_deadline() -> TestResult {
    for read_only in [false, true] {
        assert_nested_observation_deadline(CancelCellMode::ExecInFlight, 40, 20, read_only, true)?;
        assert_nested_observation_deadline(CancelCellMode::WaitWindow, 40, 4, read_only, true)?;
    }
    Ok(())
}

fn assert_nested_observation_deadline(
    mode: CancelCellMode,
    nested_seconds: u64,
    settled_seconds: u64,
    read_only: bool,
    stall_after_settlement: bool,
) -> TestResult {
    run_cancel_cell_scenario_with_timeout(8 << 20, Duration::from_secs(600), move || async move {
        let directory = tempfile::tempdir()?;
        let evidence = Arc::new(CancelCellEvidence::default());
        evidence
            .nested_deadline_fixture
            .store(true, Ordering::Release);
        evidence
            .read_only_fixture
            .store(read_only, Ordering::Release);
        let (builder, _) = cancel_cell_builder(mode, evidence.clone(), directory.path())?;
        let sessions = DurableSessionRuntime::current(
            Arc::new(TestCreateStore::default()),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("lease")?,
        )?;
        let agent = sessions
            .create(
                "create:nested-deadline",
                builder.config(
                    AgentConfig::default().with_model_operation_timeout(Duration::from_secs(30)),
                ),
            )
            .await?;
        let turn = agent.prompt("nested work outlives observation").await?;
        wait_for_scheduler_fact(&evidence.block_started).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(nested_seconds)).await;
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        assert_eq!(evidence.counters.terminates.load(Ordering::Acquire), 0);
        evidence.release_block.store(true, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !lock_mutex(&evidence.deliveries)
                .iter()
                .any(|(id, ok, _)| id == "BLOCK" && *ok)
            {
                assert_eq!(
                    evidence.counters.terminates.load(Ordering::Acquire),
                    0,
                    "expired outer deadline must not fence a settled nested mutation"
                );
                tokio::task::yield_now().await;
            }
        })
        .await?;
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(settled_seconds)).await;
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            evidence.counters.terminates.load(Ordering::Acquire),
            0,
            "settlement must start a fresh observation window"
        );
        if stall_after_settlement {
            let remaining = match mode {
                CancelCellMode::WaitWindow => 6 - settled_seconds,
                _ => 31 - settled_seconds,
            };
            tokio::time::advance(Duration::from_secs(remaining)).await;
        } else {
            evidence.release_cell.store(true, Ordering::Release);
        }
        let result = turn.result().await?;
        tokio::time::resume();
        if stall_after_settlement {
            assert!(
                matches!(result.outcome(), TurnResultOutcome::RecoveryRequired(_)),
                "{:?}",
                result.outcome()
            );
            assert_eq!(evidence.counters.terminates.load(Ordering::Acquire), 1);
            let _ = agent.shutdown().await;
        } else {
            assert!(
                matches!(result.outcome(), TurnResultOutcome::Completed(_)),
                "{:?}",
                result.outcome()
            );
            agent.shutdown().await?;
        }
        Ok(())
    })
}

#[test]
fn computing_cell_without_nested_work_still_hits_observation_deadline() -> TestResult {
    run_cancel_cell_scenario_with_timeout(8 << 20, Duration::from_secs(600), || async {
        let directory = tempfile::tempdir()?;
        let evidence = Arc::new(CancelCellEvidence::default());
        let (builder, _) = cancel_cell_builder(
            CancelCellMode::InitialResponsePending,
            evidence.clone(),
            directory.path(),
        )?;
        let sessions = DurableSessionRuntime::current(
            Arc::new(TestCreateStore::default()),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("lease")?,
        )?;
        let agent = sessions
            .create(
                "create:computing-deadline",
                builder.config(
                    AgentConfig::default().with_model_operation_timeout(Duration::from_secs(30)),
                ),
            )
            .await?;
        let turn = agent.prompt("compute indefinitely").await?;
        wait_for_scheduler_fact(&evidence.initial_response_started).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(40)).await;
        tokio::time::resume();
        let result = turn.result().await?;
        assert!(
            matches!(result.outcome(), TurnResultOutcome::RecoveryRequired(_)),
            "{:?}",
            result.outcome()
        );
        assert_eq!(evidence.counters.terminates.load(Ordering::Acquire), 1);
        let _ = agent.shutdown().await;
        Ok(())
    })
}

#[test]
fn ready_cell_terminal_beats_synthetic_steer_yield() -> TestResult {
    run_cancel_cell_scenario(|| async {
        let directory = tempfile::tempdir()?;
        let evidence = Arc::new(CancelCellEvidence::default());
        let agent = cancel_cell_agent(
            CancelCellMode::InitialResponsePending,
            evidence.clone(),
            directory.path(),
        )
        .await?;
        let turn = agent.prompt("finish cell").await?;
        wait_for_scheduler_fact(&evidence.initial_response_started).await;
        evidence.release_cell.store(true, Ordering::Release);
        turn.control().steer("new instruction").await?;
        let result = turn.result().await?;
        assert!(result.conversation().items().iter().flat_map(ConversationItem::blocks).any(|block|
            matches!(block, ContentBlock::ToolResult(tool) if tool.parts().iter().any(|part|
                matches!(part, ToolResultPart::Text(text) if text.text().contains("cell settled"))))));
        assert_eq!(evidence.counters.terminates.load(Ordering::Acquire), 0);
        agent.shutdown().await?;
        Ok(())
    })
}

#[test]
fn buffered_python_termination_preserves_the_driver_owned_generation() -> TestResult {
    run_cancel_cell_scenario(|| async {
        let directory = tempfile::tempdir()?;
        let evidence = Arc::new(CancelCellEvidence::default());
        evidence.python_fixture.store(true, Ordering::Release);
        evidence
            .buffered_terminate_fixture
            .store(true, Ordering::Release);
        let agent = cancel_cell_agent(
            CancelCellMode::WaitWindow,
            evidence.clone(),
            directory.path(),
        )
        .await?;
        let result = agent
            .prompt("observe finished Python cell")
            .await?
            .result()
            .await?;
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{:?}",
            result.outcome()
        );
        assert_eq!(evidence.counters.terminates.load(Ordering::Acquire), 1);
        assert_eq!(
            evidence.counters.session_shutdowns.load(Ordering::Acquire),
            0,
            "a buffered result must not reset the session-owned interpreter"
        );
        assert!(result.conversation().items().iter().flat_map(ConversationItem::blocks).any(|block|
            matches!(block, ContentBlock::ToolResult(tool) if tool.parts().iter().any(|part|
                matches!(part, ToolResultPart::Text(text) if text.text().contains("buffered result"))))));
        agent.shutdown().await?;
        Ok(())
    })
}

/// A cell whose nested call settled and which then stalls past its observation
/// window ends `RecoveryRequired`; the root-boundary cleanup then terminates
/// the cell and finalizes its evaluation, so the head keeps only the settled
/// child, its effect and the round's provider attempt as finalization debt.
/// No continuation names the finalized evaluation, but the child does. The
/// reopen must close that head from the recorded terminal, not refuse it as
/// integrity, or the fenced child's session can never be reopened again.
#[test]
fn reopen_closes_a_stalled_cell_head_whose_settled_child_names_the_evaluation() -> TestResult {
    run_cancel_cell_scenario_with_timeout(8 << 20, Duration::from_secs(600), || async {
        let directory = tempfile::tempdir()?;
        let evidence = Arc::new(CancelCellEvidence::default());
        evidence
            .nested_deadline_fixture
            .store(true, Ordering::Release);
        let store = Arc::new(TestCreateStore::default());
        let sessions = DurableSessionRuntime::current(
            store.clone(),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("lease")?,
        )?;
        let (builder, _) = cancel_cell_builder(
            CancelCellMode::ExecInFlight,
            evidence.clone(),
            directory.path(),
        )?;
        let agent = sessions
            .create(
                "create:stalled-cell-reopen",
                builder.config(
                    AgentConfig::default().with_model_operation_timeout(Duration::from_secs(30)),
                ),
            )
            .await?;
        let turn = agent.prompt("nested work then a stalled cell").await?;
        wait_for_scheduler_fact(&evidence.block_started).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(40)).await;
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        evidence.release_block.store(true, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !lock_mutex(&evidence.deliveries)
                .iter()
                .any(|(id, ok, _)| id == "BLOCK" && *ok)
            {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(31)).await;
        let result = turn.result().await?;
        tokio::time::resume();
        assert!(
            matches!(result.outcome(), TurnResultOutcome::RecoveryRequired(_)),
            "{:?}",
            result.outcome()
        );
        let head = agent.checkpoint();
        let RuntimeLifecycle::Working { obligations, .. } = head.manifest().lifecycle() else {
            return Err("the stalled cell left no working head".into());
        };
        assert!(
            obligations.iter().any(|obligation| obligation.kind()
                == RecoveryObligationKind::CodeCell
                && obligation.action() == RecoveryAction::FinalizationRequired),
            "the settled child stays as Code Cell finalization debt\n{}",
            describe_ledger(&result)
        );
        assert_ne!(
            crate::runtime::checkpoint_recovery_verdict(&head),
            crate::runtime::CheckpointRecoveryVerdict::Refuse,
            "a head the live driver wrote is never refused as integrity\n{}",
            describe_ledger(&result)
        );
        let source = DurableSessionAddress::new(agent.session_id(), agent.branch_id(), head.id());
        let _ = agent.shutdown().await;
        lock_mutex(&store.state).active_lease = None;
        let reopening = DurableSessionRuntime::current(
            store.clone(),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("lease")?,
        )?;
        let (builder, model) = cancel_cell_builder(
            CancelCellMode::ExecInFlight,
            evidence.clone(),
            directory.path(),
        )?;
        let reopened = match reopening
            .open("open:stalled-cell-reopen", source, builder)
            .await?
        {
            DurableRuntimeOpen::Opened(agent) => agent,
            DurableRuntimeOpen::RecoveryRequired { .. } => {
                return Err("the reopen refused the stalled cell head".into());
            }
            DurableRuntimeOpen::RestoredWorking { .. } => {
                return Err("a terminated cell was restored".into());
            }
        };
        assert!(matches!(
            reopened.checkpoint().manifest().lifecycle(),
            RuntimeLifecycle::Quiescent
        ));
        assert_eq!(
            model.calls.load(Ordering::Acquire),
            0,
            "nothing is replayed"
        );
        reopened.shutdown().await?;
        Ok(())
    })
}
