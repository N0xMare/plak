use super::*;

struct NoticeFixture {
    directory: tempfile::TempDir,
    agent: Agent,
    sessions: DurableSessionRuntime,
    store: Arc<TestCreateStore>,
    model: Arc<ArtifactModel>,
    registry: FrozenToolRegistry,
    tool: Arc<SessionUpdateTool>,
    release: Arc<Notify>,
}

impl NoticeFixture {
    async fn new(held_call: usize, continuations: &[usize]) -> TestResult<Self> {
        Self::with_terminal_poll(held_call, continuations, false).await
    }

    async fn with_terminal_poll(
        held_call: usize,
        continuations: &[usize],
        terminal_poll: bool,
    ) -> TestResult<Self> {
        let directory = tempfile::tempdir()?;
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
            .with_assistant_continuations(continuations.iter().copied())
            .with_scripted_tool_calls(terminal_poll.then_some((1, "probe", "{}".to_owned()))),
        );
        let release = Arc::new(Notify::new());
        *lock_mutex(&model.held_call_release) = Some(release.clone());
        model.held_call.store(held_call, Ordering::Release);
        let (registry, tool) = session_update_registry(SessionUpdateFixtureMode::Promoted)?;
        tool.await_promoted_terminal
            .store(terminal_poll, Ordering::Release);
        let store = Arc::new(TestCreateStore::default());
        let sessions = DurableSessionRuntime::current(
            store.clone(),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("lease")?,
        )?;
        let agent = sessions
            .create(
                "create:notice-boundary",
                session_update_builder(
                    Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
                    model.clone(),
                    registry.clone(),
                )?,
            )
            .await?;
        Ok(Self {
            directory,
            agent,
            sessions,
            store,
            model,
            registry,
            tool,
            release,
        })
    }

    async fn held(&self) -> TestResult {
        tokio::time::timeout(
            Duration::from_secs(5),
            self.model.held_call_started.notified(),
        )
        .await?;
        Ok(())
    }

    fn stop(&self) -> TestResult {
        let service = lock_mutex(&self.tool.sessions)
            .clone()
            .ok_or("process service")?;
        let id = service.session_for_number(1).ok_or("process handle")?;
        service.stop(id, plakous_tools::JobStopReason::User)?;
        Ok(())
    }

    async fn reopen(&self) -> TestResult<Agent> {
        self.agent.abort_driver_for_test();
        self.store.expire_active_lease();
        self.store
            .block_process_notice_commits
            .store(false, Ordering::Release);
        let address = DurableSessionAddress::new(
            self.agent.session_id(),
            self.agent.branch_id(),
            self.agent.checkpoint().id(),
        );
        let opened = self
            .sessions
            .open(
                "open:notice",
                address,
                session_update_builder(
                    Arc::new(WorkspacePolicy::host(self.directory.path(), ".")?),
                    self.model.clone(),
                    self.registry.clone(),
                )?,
            )
            .await?;
        match opened {
            DurableRuntimeOpen::Opened(agent) => Ok(agent),
            DurableRuntimeOpen::RecoveryRequired { .. } => {
                Err("exact reopen did not close the interrupted turn".into())
            }
            DurableRuntimeOpen::RestoredWorking { .. } => {
                Err("unexpected restored notice continuation".into())
            }
        }
    }
}

fn notice_count(agent: &Agent) -> usize {
    agent.checkpoint().manifest().canonical().history().segments().iter()
        .filter(|segment| segment.items().iter().flat_map(ConversationItem::blocks)
            .any(|block| matches!(block, ContentBlock::Text(text) if text.text().contains("<process_notice"))))
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn process_notice_terminal_tool_result_consumes_before_boundary_delivery() -> TestResult {
    let fixture = NoticeFixture::with_terminal_poll(usize::MAX, &[], true).await?;
    let turn = fixture.agent.prompt("start then wait on the job").await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while fixture.tool.dispatches.load(Ordering::Acquire) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let service = lock_mutex(&fixture.tool.sessions)
        .clone()
        .ok_or("service")?;
    let id = service.session_for_number(1).ok_or("handle")?;
    service.write_stdin(
        id,
        b"finish\n",
        plakous_tools::JoinedProcessInputDisposition::KeepOpen,
    )?;
    let result = turn.result().await?;
    if let TurnResultOutcome::RecoveryRequired(error) | TurnResultOutcome::Failed(error) =
        result.outcome()
    {
        return Err(error.message().to_owned().into());
    }
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(fixture.agent.jobs(), [] as [plakous_tools::JobView; 0]);
    assert_eq!(notice_count(&fixture.agent), 0);
    fixture
        .agent
        .prompt("no duplicate completion")
        .await?
        .result()
        .await?;
    assert_eq!(notice_count(&fixture.agent), 0);
    fixture.agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn process_notice_capacity_refusal_defers_to_next_boundary() -> TestResult {
    let fixture = NoticeFixture::new(1, &[1, 2]).await?;
    let turn = fixture.agent.prompt("start").await?;
    fixture.held().await?;
    fixture.stop()?;
    fixture.store.refuse_ordinary_commit_on.store(
        fixture.store.ordinary_commits.load(Ordering::Acquire) + 1,
        Ordering::Release,
    );
    fixture.release.notify_one();
    let result = turn.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(fixture.store.ordinary_refusals.load(Ordering::Acquire), 1);
    assert_eq!(notice_count(&fixture.agent), 1);
    assert_eq!(fixture.agent.jobs(), [] as [plakous_tools::JobView; 0]);
    fixture.agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn process_notice_crash_before_commit_redelivers_after_reopen() -> TestResult {
    let fixture = NoticeFixture::new(1, &[1]).await?;
    let turn = fixture.agent.prompt("start").await?;
    fixture.held().await?;
    fixture.stop()?;
    fixture
        .store
        .block_process_notice_commits
        .store(true, Ordering::Release);
    fixture.release.notify_one();
    tokio::time::timeout(
        Duration::from_secs(5),
        fixture.store.process_notice_commit_started.notified(),
    )
    .await?;
    assert_eq!(notice_count(&fixture.agent), 0);
    assert_eq!(fixture.agent.checkpoint().manifest().jobs().rows().len(), 1);
    let reopened = fixture.reopen().await?;
    reopened
        .prompt("continue after crash")
        .await?
        .result()
        .await?;
    assert_eq!(notice_count(&reopened), 1);
    assert_eq!(reopened.jobs(), [] as [plakous_tools::JobView; 0]);
    drop(turn);
    reopened.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn process_notice_crash_after_commit_never_redelivers() -> TestResult {
    let fixture = NoticeFixture::new(1, &[1]).await?;
    let turn = fixture.agent.prompt("start").await?;
    fixture.held().await?;
    fixture.stop()?;
    fixture.release.notify_one();
    turn.result().await?;
    assert_eq!(notice_count(&fixture.agent), 1);
    let reopened = fixture.reopen().await?;
    assert_eq!(reopened.jobs(), [] as [plakous_tools::JobView; 0]);
    reopened
        .prompt("continue after committed notice")
        .await?
        .result()
        .await?;
    assert_eq!(notice_count(&reopened), 1);
    reopened.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn process_notice_boundary_without_steering_commits_once() -> TestResult {
    let fixture = NoticeFixture::new(1, &[1]).await?;
    let turn = fixture.agent.prompt("start").await?;
    fixture.held().await?;
    fixture.stop()?;
    fixture.release.notify_one();
    let result = turn.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(notice_count(&fixture.agent), 1);
    assert_eq!(fixture.agent.jobs(), [] as [plakous_tools::JobView; 0]);
    assert!(
        lock_mutex(&fixture.model.observed_request_texts)
            .iter()
            .any(|texts| texts.iter().any(|text| text.contains("<process_notice")))
    );
    fixture.agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn process_notice_rides_pending_steering_commit() -> TestResult {
    let fixture = NoticeFixture::new(1, &[]).await?;
    let turn = fixture.agent.prompt("start").await?;
    fixture.held().await?;
    fixture.stop()?;
    turn.control().steer("change course").await?;
    fixture.release.notify_one();
    let result = turn.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(notice_count(&fixture.agent), 1);
    assert_eq!(fixture.agent.jobs(), [] as [plakous_tools::JobView; 0]);
    {
        let state = lock_mutex(&fixture.store.state);
        let first_notice = state.stored_commits.values()
            .filter(|stored| stored.checkpoint().manifest().canonical().history().segments().iter()
                .any(|segment| segment.items().iter().flat_map(ConversationItem::blocks)
                    .any(|block| matches!(block, ContentBlock::Text(text) if text.text().contains("<process_notice")))))
            .min_by_key(|stored| stored.checkpoint().manifest().topology().branch_revision)
            .ok_or("notice commit")?;
        assert_eq!(first_notice.checkpoint().manifest().jobs().rows(), []);
        assert!(first_notice.checkpoint().manifest().canonical().history().segments().iter()
            .flat_map(HistorySegment::items).flat_map(ConversationItem::blocks)
            .any(|block| matches!(block, ContentBlock::Text(text) if text.text() == "change course")));
    }
    fixture.agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn process_notice_does_not_extend_final_answer_and_child_instruction_admits_it() -> TestResult
{
    let fixture = NoticeFixture::new(1, &[]).await?;
    let turn = fixture.agent.prompt("start").await?;
    fixture.held().await?;
    fixture.stop()?;
    fixture.release.notify_one();
    turn.result().await?;
    assert_eq!(fixture.model.calls.load(Ordering::Acquire), 2);
    assert_eq!(notice_count(&fixture.agent), 0);
    assert_eq!(fixture.agent.jobs().len(), 1);
    let submission = fixture
        .agent
        .start_instruction(CommandId::new("child:nudge")?, "resume child task")
        .await?;
    submission
        .into_turn()
        .ok_or("child admission")?
        .result()
        .await?;
    assert_eq!(notice_count(&fixture.agent), 1);
    assert_eq!(fixture.agent.jobs(), [] as [plakous_tools::JobView; 0]);
    fixture.agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn a_host_notice_observed_before_a_consumer_boundary_is_stale() -> TestResult {
    let fixture = NoticeFixture::new(1, &[]).await?;
    let observed = fixture.agent.checkpoint().id();
    let turn = fixture
        .agent
        .prompt("consume a tool result before host notification")
        .await?;
    fixture.held().await?;
    assert_ne!(fixture.agent.checkpoint().id(), observed);
    let reply = turn.control().notice_at(observed)?.await?;
    assert_eq!(
        reply.expect_err("stale host snapshot was accepted").kind(),
        ControlErrorKind::Stale
    );
    fixture.release.notify_one();
    turn.result().await?;
    fixture.agent.shutdown().await?;
    Ok(())
}

struct ObservedJobSiblingTool {
    setup: Arc<SessionUpdateTool>,
    calls: AtomicUsize,
    joined: AtomicBool,
    release: AtomicBool,
    sibling_preempted: AtomicBool,
}

impl plakous_tools::ToolExecutor for ObservedJobSiblingTool {
    fn execute(
        self: Arc<Self>,
        invocation: ToolInvocation,
        context: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        Box::pin(async move {
            match self.calls.fetch_add(1, Ordering::AcqRel) {
                0 => {
                    let outcome = Arc::clone(&self.setup).execute(invocation, context).await?;
                    let sessions = lock_mutex(&self.setup.sessions).clone().ok_or_else(|| {
                        ToolRuntimeError::new(
                            ToolRuntimeErrorKind::Execution,
                            "missing process service",
                        )
                    })?;
                    let id = sessions.session_for_number(1).ok_or_else(|| {
                        ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, "missing process")
                    })?;
                    let (_, _, state) =
                        plakous_tools::internal::process_session_projection_state(&sessions, id)
                            .map_err(plakous_tools::backend_runtime_error)?;
                    state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .completion_notices = true;
                    Ok(outcome)
                }
                1 => {
                    let result = ProcessSessionTools::await_jobs(&context, &[1], 600_000).await?;
                    self.joined.store(true, Ordering::Release);
                    Ok(result)
                }
                _ => {
                    while !self.release.load(Ordering::Acquire) {
                        if *context.preempt().borrow() != plakous_tools::ToolPreempt::None {
                            self.sibling_preempted.store(true, Ordering::Release);
                        }
                        tokio::task::yield_now().await;
                    }
                    ToolOutcome::success_text("sibling finished without a spurious notice")
                }
            }
        })
    }
}

#[tokio::test]
async fn a_parallel_observer_join_does_not_preempt_its_waiting_sibling() -> TestResult {
    let directory = tempfile::tempdir()?;
    let descriptor = durable_test_descriptor("test.route")?;
    let mut fixture = ArtifactModel::new(
        descriptor.clone(),
        artifact_authority(&descriptor, 1)?,
        0,
        94_950,
        false,
    )
    .with_scripted_tool_calls([(0, "probe", "{}".into()), (1, "probe", "{}".into())])
    .with_second_direct_tool()?;
    fixture.secondary_tool_on = Some(1);
    let model = Arc::new(fixture);
    let tool = Arc::new(ObservedJobSiblingTool {
        setup: Arc::new(SessionUpdateTool::new(SessionUpdateFixtureMode::Promoted)),
        calls: AtomicUsize::new(0),
        joined: AtomicBool::new(false),
        release: AtomicBool::new(false),
        sibling_preempted: AtomicBool::new(false),
    });
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("observed-job-sibling.probe")?, ToolExecutorId::new("observed-job-sibling.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"observed-job-sibling"] )?,
        ToolDefinition::new(ToolName::new("probe")?, "1.0.0", "Observe a process or await a sibling.",
            ToolInputContract::structured(json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{},"required":[],"additionalProperties":false}))?,
            ToolOutputContract::model_content(128, 1_048_576)?, ToolConcurrency::Parallel, ToolEffect::ReadOnly)?, tool.clone(),
    ))?;
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:observed-job-sibling",
            session_update_builder(
                Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
                model,
                registry.freeze(),
            )?,
        )
        .await?;
    let turn = agent
        .prompt("join the job without waking its sibling")
        .await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if tool.calls.load(Ordering::Acquire) >= 3 {
                let service = lock_mutex(&tool.setup.sessions)
                    .clone()
                    .ok_or("missing service")?;
                let id = service.session_for_number(1).ok_or("missing session")?;
                let (_, _, state) =
                    plakous_tools::internal::process_session_projection_state(&service, id)?;
                if state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .observers
                    > 0
                {
                    service.write_stdin(
                        id,
                        b"finish\n",
                        plakous_tools::JoinedProcessInputDisposition::KeepOpen,
                    )?;
                    break;
                }
            }
            tokio::task::yield_now().await;
        }
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await??;
    tokio::time::timeout(
        Duration::from_secs(10),
        wait_for_scheduler_fact(&tool.joined),
    )
    .await?;
    for _ in 0..128 {
        tokio::task::yield_now().await;
    }
    assert!(
        !tool.sibling_preempted.load(Ordering::Acquire),
        "an in-band join preempted its read-only sibling"
    );
    tool.release.store(true, Ordering::Release);
    assert!(matches!(
        turn.result().await?.outcome(),
        TurnResultOutcome::Completed(_)
    ));
    agent.shutdown().await?;
    Ok(())
}
