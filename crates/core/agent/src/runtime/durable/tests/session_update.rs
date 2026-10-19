use super::*;

pub(super) fn one_tool_registry() -> TestResult<FrozenToolRegistry> {
    one_tool_registry_owned_by(plakous_tools::ToolOwnerClass::PlakLocal)
}

pub(super) fn one_tool_registry_owned_by(
    owner: plakous_tools::ToolOwnerClass,
) -> TestResult<FrozenToolRegistry> {
    let definition = ToolDefinition::new(
        ToolName::new("probe")?,
        "1.0.0",
        "Probe one value.",
        ToolInputContract::structured(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }))?,
        ToolOutputContract::model_content(128, 1_048_576)?,
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )?;
    let mut registry = ToolRegistryBuilder::default();
    registry.register(
        ToolRegistration::new(
            RegisteredToolId::new("probe.registration")?,
            ToolExecutorId::new("probe.executor")?,
            plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"probe.executor"])?,
            definition.clone(),
            Arc::new(plakous_test_support::ScriptedTool::new([
                ToolOutcome::success_text("ok"),
            ])),
        )
        .with_owner(owner),
    )?;
    Ok(registry.freeze())
}

pub(super) fn one_mutating_tool_registry() -> TestResult<FrozenToolRegistry> {
    mutating_tool_registry_with_outcomes(1)
}

pub(super) fn mutating_tool_registry_with_outcomes(
    outcomes: usize,
) -> TestResult<FrozenToolRegistry> {
    mutating_tool_registry_with_results((0..outcomes).map(|_| ToolOutcome::success_text("ok")))
}

pub(super) fn mutating_tool_registry_with_results(
    outcomes: impl IntoIterator<Item = Result<ToolOutcome, ToolRuntimeError>>,
) -> TestResult<FrozenToolRegistry> {
    let definition = ToolDefinition::new(
        ToolName::new("probe")?,
        "1.0.0",
        "Mutate one value.",
        ToolInputContract::structured(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }))?,
        ToolOutputContract::model_content(128, 1_048_576)?,
        ToolConcurrency::Serial,
        ToolEffect::Mutating,
    )?;
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("probe.registration")?,
        ToolExecutorId::new("probe.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"probe.executor"])?,
        definition,
        Arc::new(plakous_test_support::ScriptedTool::new(outcomes)),
    ))?;
    Ok(registry.freeze())
}

#[derive(Clone, Copy)]
pub(in crate::runtime::durable) enum SessionUpdateFixtureMode {
    Active,
    Promoted,
    ExitedRetained,
    ExitedPaginated,
    ActiveThenJoinedForeground,
    ActiveThenJoinedForegroundStale,
    ActiveAfterCancellation,
    JoinedForeground,
    PostStartError,
}

pub(super) struct SessionUpdateTool {
    pub(super) await_promoted_terminal: AtomicBool,
    pub(super) mode: SessionUpdateFixtureMode,
    pub(super) wait_for_preempt: AtomicBool,
    pub(super) preempt_ready: Notify,
    pub(super) check_preempt_reset: AtomicBool,
    pub(super) dispatches: AtomicUsize,
    pub(super) sessions: Mutex<Option<plakous_tools::JoinedProcessSessions>>,
    pub(super) first_session_id: Mutex<Option<plakous_tools::JoinedProcessSessionId>>,
    pub(super) page_cursor: AtomicU64,
}

impl SessionUpdateTool {
    pub(super) fn new(mode: SessionUpdateFixtureMode) -> Self {
        Self {
            await_promoted_terminal: AtomicBool::new(false),
            mode,
            wait_for_preempt: AtomicBool::new(false),
            preempt_ready: Notify::new(),
            check_preempt_reset: AtomicBool::new(false),
            dispatches: AtomicUsize::new(0),
            sessions: Mutex::new(None),
            first_session_id: Mutex::new(None),
            page_cursor: AtomicU64::new(0),
        }
    }
}

impl plakous_tools::ToolExecutor for SessionUpdateTool {
    fn execute(
        self: Arc<Self>,
        _invocation: ToolInvocation,
        context: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        Box::pin(async move {
            let dispatch = self.dispatches.fetch_add(1, Ordering::AcqRel);
            if self.check_preempt_reset.load(Ordering::Acquire)
                || (self.wait_for_preempt.load(Ordering::Acquire) && dispatch > 0)
            {
                assert_eq!(
                    *context.preempt().borrow(),
                    plakous_tools::ToolPreempt::None
                );
                return ToolOutcome::success_text("preempt reset");
            }
            let mode = if matches!(
                self.mode,
                SessionUpdateFixtureMode::ActiveThenJoinedForeground
                    | SessionUpdateFixtureMode::ActiveThenJoinedForegroundStale
            ) {
                if dispatch == 0 {
                    SessionUpdateFixtureMode::Active
                } else {
                    SessionUpdateFixtureMode::JoinedForeground
                }
            } else {
                self.mode
            };
            let sessions = context
                .environment()
                .joined_process_sessions()
                .map_err(|_| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Execution,
                        "fixture joined service failed",
                    )
                })?;
            if matches!(mode, SessionUpdateFixtureMode::Promoted) && dispatch > 0 {
                let view = sessions.jobs().into_iter().next().ok_or_else(|| {
                    ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, "fixture job missing")
                })?;
                let id = sessions.session_for_number(view.number).ok_or_else(|| {
                    ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, "fixture handle missing")
                })?;
                if self.await_promoted_terminal.load(Ordering::Acquire) {
                    while sessions.joined_terminal_status(id).is_none() {
                        tokio::task::yield_now().await;
                    }
                }
                let status = sessions
                    .join(id)
                    .map_err(plakous_tools::backend_runtime_error)?;
                let outcome = ToolOutcome::success_text(format!("{status:?}: {}", view.output))?;
                return Ok(plakous_tools::internal::process_session_joined(
                    outcome, &sessions, id,
                ));
            }
            let shell =
                plakous_tools::JoinedProcessShellProfile::new("/bin/sh", [String::from("-c")])
                    .map_err(|_| {
                        ToolRuntimeError::new(
                            ToolRuntimeErrorKind::Execution,
                            "fixture shell profile failed",
                        )
                    })?;
            if matches!(
                self.mode,
                SessionUpdateFixtureMode::ActiveThenJoinedForegroundStale
            ) && dispatch == plakous_tools::MAX_JOINED_PROCESS_SESSIONS * 2 + 8
            {
                let id = (*lock_mutex(&self.first_session_id)).ok_or_else(|| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Execution,
                        "stale fixture lost original process identity",
                    )
                })?;
                let outcome = plakous_tools::internal::process_session_active(
                    ToolOutcome::success_text("delayed active page")?,
                    &sessions,
                    id,
                );
                let Some(plakous_tools::internal::ProcessSessionUpdate::Active {
                    session_id, ..
                }) = plakous_tools::internal::process_session_update(&outcome)
                else {
                    unreachable!("fixture just constructed an Active sideband")
                };
                if plakous_tools::internal::joined_process_session_identities(&sessions)
                    .iter()
                    .any(|(joined_id, _)| *joined_id == session_id)
                {
                    return Err(ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Execution,
                        "stale fixture did not evict backend joined proof",
                    ));
                }
                return Ok(outcome);
            }
            if matches!(mode, SessionUpdateFixtureMode::ExitedPaginated) {
                let id = if dispatch == 0 {
                    let request = plakous_tools::JoinedProcessStartRequest::new(
                        plakous_tools::JoinedProcessCommand::shell(shell, "printf '%064d' 0")
                            .map_err(|_| {
                                ToolRuntimeError::new(
                                    ToolRuntimeErrorKind::Execution,
                                    "paged fixture process command failed",
                                )
                            })?,
                        ".",
                        Vec::new(),
                        false,
                        plakous_tools::JoinedProcessTerminalMode::Pipes,
                    )
                    .map_err(|_| {
                        ToolRuntimeError::new(
                            ToolRuntimeErrorKind::Execution,
                            "paged fixture process request failed",
                        )
                    })?;
                    let id = sessions
                        .start(
                            request,
                            plakous_tools::ExecutionOperationControl::new(
                                context.cancellation().clone(),
                            ),
                        )
                        .map_err(|_| {
                            ToolRuntimeError::new(
                                ToolRuntimeErrorKind::Execution,
                                "paged fixture process start failed",
                            )
                        })?;
                    *lock_mutex(&self.first_session_id) = Some(id);
                    *lock_mutex(&self.sessions) = Some(sessions.clone());
                    id
                } else {
                    (*lock_mutex(&self.first_session_id)).ok_or_else(|| {
                        ToolRuntimeError::new(
                            ToolRuntimeErrorKind::Execution,
                            "paged fixture lost process identity",
                        )
                    })?
                };
                tokio::time::timeout(Duration::from_secs(5), async {
                    while sessions.joined_terminal_status(id).is_none() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .map_err(|_| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Execution,
                        "paged fixture process did not exit",
                    )
                })?;
                let cursor = plakous_tools::JoinedProcessOutputCursor::new(
                    self.page_cursor.load(Ordering::Acquire),
                );
                let page = sessions.poll_from(id, cursor, 16, 256).map_err(|_| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Execution,
                        "paged fixture poll failed",
                    )
                })?;
                if !page.status().is_terminal()
                    || page
                        .records()
                        .iter()
                        .map(|record| record.bytes().len())
                        .sum::<usize>()
                        != 16
                {
                    return Err(ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Execution,
                        "paged fixture did not receive one terminal 16-byte page",
                    ));
                }
                self.page_cursor
                    .store(page.next_cursor().get(), Ordering::Release);
                let outcome = ToolOutcome::success_text("terminal child output page")?;
                return if page.more_output() {
                    Ok(plakous_tools::internal::process_session_active(
                        outcome, &sessions, id,
                    ))
                } else {
                    sessions.terminate(id).map_err(|_| {
                        ToolRuntimeError::new(
                            ToolRuntimeErrorKind::Execution,
                            "paged fixture final join failed",
                        )
                    })?;
                    Ok(plakous_tools::internal::process_session_joined(
                        outcome, &sessions, id,
                    ))
                };
            }
            if matches!(
                self.mode,
                SessionUpdateFixtureMode::ActiveThenJoinedForeground
                    | SessionUpdateFixtureMode::ActiveThenJoinedForegroundStale
            ) && dispatch == 1
            {
                let earlier = (*lock_mutex(&self.first_session_id)).ok_or_else(|| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Execution,
                        "fixture lost earlier active child identity",
                    )
                })?;
                sessions.terminate(earlier).map_err(|_| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Execution,
                        "fixture could not retire earlier active child",
                    )
                })?;
            }
            let command = match mode {
                SessionUpdateFixtureMode::Promoted => {
                    // Shutdown closes stdin before the stop signal. `read` then
                    // returns on EOF, and a command that continues into `exit 7`
                    // records Exited(7) instead of Stopped(SessionExit). Only a
                    // completed line takes that path; EOF sleeps until the signal.
                    "while :; do if read line; then printf 'terminal\\000tail'; exit 7; fi; sleep 60; done"
                }
                SessionUpdateFixtureMode::JoinedForeground
                | SessionUpdateFixtureMode::ExitedRetained => "true",
                SessionUpdateFixtureMode::ExitedPaginated
                | SessionUpdateFixtureMode::ActiveThenJoinedForeground
                | SessionUpdateFixtureMode::ActiveThenJoinedForegroundStale => unreachable!(),
                SessionUpdateFixtureMode::Active
                | SessionUpdateFixtureMode::ActiveAfterCancellation
                | SessionUpdateFixtureMode::PostStartError => "sleep 60",
            };
            let request = plakous_tools::JoinedProcessStartRequest::new(
                plakous_tools::JoinedProcessCommand::shell(shell, command).map_err(|_| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Execution,
                        "fixture process command failed",
                    )
                })?,
                ".",
                Vec::new(),
                matches!(mode, SessionUpdateFixtureMode::Promoted),
                plakous_tools::JoinedProcessTerminalMode::Pipes,
            )
            .map_err(|_| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    "fixture process request failed",
                )
            })?;
            let id = sessions
                .start(
                    request,
                    plakous_tools::ExecutionOperationControl::new(
                        if matches!(mode, SessionUpdateFixtureMode::ActiveAfterCancellation) {
                            CancellationToken::new()
                        } else {
                            context.cancellation().clone()
                        },
                    ),
                )
                .map_err(|_| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Execution,
                        "fixture process start failed",
                    )
                })?;
            if matches!(
                self.mode,
                SessionUpdateFixtureMode::ActiveThenJoinedForeground
                    | SessionUpdateFixtureMode::ActiveThenJoinedForegroundStale
            ) && dispatch == 0
            {
                *lock_mutex(&self.first_session_id) = Some(id);
            }
            *lock_mutex(&self.sessions) = Some(sessions.clone());
            if self.wait_for_preempt.load(Ordering::Acquire)
                && self.dispatches.load(Ordering::Acquire) == 1
            {
                self.preempt_ready.notify_one();
                let mut preempt = context.preempt().clone();
                while *preempt.borrow_and_update() == plakous_tools::ToolPreempt::None {
                    tokio::select! {
                        changed = preempt.changed() => {
                            if changed.is_err() {
                                context.cancellation().cancelled().await;
                                break;
                            }
                        }
                        () = context.cancellation().cancelled() => break,
                    }
                }
            }
            if matches!(mode, SessionUpdateFixtureMode::ExitedRetained) {
                tokio::time::timeout(Duration::from_secs(5), async {
                    while sessions.joined_terminal_status(id).is_none() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .map_err(|_| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Execution,
                        "fixture child did not exit before the next command",
                    )
                })?;
            }
            if matches!(mode, SessionUpdateFixtureMode::PostStartError) {
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    "fixture failed after process start before sideband publication",
                ));
            }
            if matches!(mode, SessionUpdateFixtureMode::ActiveAfterCancellation) {
                context.cancellation().cancelled().await;
            }
            let outcome = ToolOutcome::success_text("ok")?;
            Ok(match mode {
                SessionUpdateFixtureMode::Promoted => {
                    plakous_tools::internal::promote(&sessions, id).map_err(|_| {
                        ToolRuntimeError::new(
                            ToolRuntimeErrorKind::Execution,
                            "fixture promotion failed",
                        )
                    })?;
                    plakous_tools::internal::process_session_active(outcome, &sessions, id)
                }
                SessionUpdateFixtureMode::Active
                | SessionUpdateFixtureMode::ExitedRetained
                | SessionUpdateFixtureMode::ActiveAfterCancellation => {
                    plakous_tools::internal::process_session_active(outcome, &sessions, id)
                }
                SessionUpdateFixtureMode::JoinedForeground => {
                    sessions.terminate(id).map_err(|_| {
                        ToolRuntimeError::new(
                            ToolRuntimeErrorKind::Execution,
                            "fixture process join failed",
                        )
                    })?;
                    plakous_tools::internal::process_session_joined(outcome, &sessions, id)
                }
                SessionUpdateFixtureMode::PostStartError => {
                    unreachable!("post-start fixture returns before constructing an outcome")
                }
                SessionUpdateFixtureMode::ActiveThenJoinedForeground
                | SessionUpdateFixtureMode::ActiveThenJoinedForegroundStale
                | SessionUpdateFixtureMode::ExitedPaginated => unreachable!(),
            })
        })
    }
}

pub(super) fn session_update_registry(
    mode: SessionUpdateFixtureMode,
) -> TestResult<(FrozenToolRegistry, Arc<SessionUpdateTool>)> {
    let definition = ToolDefinition::new(
        ToolName::new("probe")?,
        "1.0.0",
        "Exercise one typed process-session update.",
        ToolInputContract::structured(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }))?,
        ToolOutputContract::model_content(128, 1_048_576)?,
        ToolConcurrency::Serial,
        ToolEffect::Mutating,
    )?;
    let tool = Arc::new(SessionUpdateTool::new(mode));
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("probe.registration")?,
        ToolExecutorId::new("probe.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"probe.executor"])?,
        definition,
        tool.clone(),
    ))?;
    Ok((registry.freeze(), tool))
}

pub(super) fn session_update_builder(
    workspace: Arc<WorkspacePolicy>,
    model: Arc<ArtifactModel>,
    registry: FrozenToolRegistry,
) -> TestResult<AgentBuilder> {
    let descriptor = model.descriptor.clone();
    let authority = model.authority.clone();
    let configured = configured_test_model(
        model,
        descriptor,
        authority,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    Ok(AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        ))
}

#[tokio::test]
async fn steer_reaches_top_level_tool_without_cancelling_the_process() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            93_700,
            false,
        )
        .with_tool_first()
        .with_scripted_tool_calls([(1, "probe", "{}".to_owned()), (3, "probe", "{}".to_owned())]),
    );
    let (registry, tool) = session_update_registry(SessionUpdateFixtureMode::Promoted)?;
    tool.wait_for_preempt.store(true, Ordering::Release);
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(
            "create:steer-preempt",
            session_update_builder(workspace, model, registry)?,
        )
        .await?;
    let turn = agent.prompt("start process").await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while lock_mutex(&tool.sessions).is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let control = turn.control();
    control.steer("new instruction").await?;
    let result = tokio::time::timeout(Duration::from_secs(2), turn.result()).await;
    let checkpoint = agent.checkpoint();
    if result.is_err() {
        control.cancel().await?;
        let _ = agent.shutdown().await;
    }
    assert!(result.is_ok(), "steer did not preempt the foreground tool");
    assert!(matches!(
        result??.outcome(),
        TurnResultOutcome::Completed(_)
    ));
    assert_eq!(
        tool.dispatches.load(Ordering::Acquire),
        2,
        "second batch in the steered turn was not dispatched"
    );
    assert_eq!(checkpoint.manifest().jobs().rows().len(), 1);
    tool.check_preempt_reset.store(true, Ordering::Release);
    let next = agent.prompt("continue").await?.result().await?;
    agent.shutdown().await?;
    assert!(
        matches!(next.outcome(), TurnResultOutcome::Completed(_)),
        "next turn: {:?}",
        next.outcome()
    );
    assert_eq!(
        tool.dispatches.load(Ordering::Acquire),
        3,
        "steer flag leaked into the next turn"
    );
    Ok(())
}

#[tokio::test]
async fn background_reaches_only_in_flight_top_level_dispatches() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            93_700,
            false,
        )
        .with_tool_first()
        .with_scripted_tool_calls([(1, "probe", "{}".to_owned()), (3, "probe", "{}".to_owned())]),
    );
    let (registry, tool) = session_update_registry(SessionUpdateFixtureMode::Promoted)?;
    tool.wait_for_preempt.store(true, Ordering::Release);
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(
            "create:background-preempt",
            session_update_builder(workspace, model, registry)?,
        )
        .await?;
    let turn = agent.prompt("start process").await?;
    tool.preempt_ready.notified().await;
    let control = turn.control();
    control.background().await?;
    let result = turn.result().await?;
    let checkpoint = agent.checkpoint();
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(
        tool.dispatches.load(Ordering::Acquire),
        2,
        "background suppressed or preempted a later dispatch"
    );
    assert_eq!(checkpoint.manifest().jobs().rows().len(), 1);
    tool.check_preempt_reset.store(true, Ordering::Release);
    let next = agent.prompt("continue").await?.result().await?;
    agent.shutdown().await?;
    assert!(
        matches!(next.outcome(), TurnResultOutcome::Completed(_)),
        "next turn: {:?}",
        next.outcome()
    );
    assert_eq!(
        tool.dispatches.load(Ordering::Acquire),
        3,
        "background edge leaked into the next turn"
    );
    Ok(())
}

#[tokio::test]
async fn mid_response_steer_supersedes_the_whole_undispatched_batch() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            93_800,
            false,
        )
        .with_tool_first()
        .with_second_direct_tool()?,
    );
    let release = Arc::new(Notify::new());
    *lock_mutex(&model.held_call_release) = Some(release.clone());
    model.held_call.store(0, Ordering::Release);
    let (registry, tool) = session_update_registry(SessionUpdateFixtureMode::Active)?;
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(
            "create:steer-superseded",
            session_update_builder(workspace, model.clone(), registry)?,
        )
        .await?;
    let turn = agent.prompt("start processes").await?;
    tokio::time::timeout(Duration::from_secs(2), model.held_call_started.notified()).await?;
    turn.control().steer("new instruction").await?;
    release.notify_one();
    let result = turn.result().await?;
    agent.shutdown().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(tool.dispatches.load(Ordering::Acquire), 0);
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
    assert!(
        results
            .iter()
            .all(|result| result.parts().iter().any(|part| matches!(part,
        ToolResultPart::Text(text) if text.text() == "not run: superseded by your message")))
    );
    let effects = result
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter(|op| matches!(op.kind(), DurableOperationKind::Effect { .. }))
        .collect::<Vec<_>>();
    assert_eq!(effects.len(), 2);
    for (index, op) in effects.iter().enumerate() {
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

    Ok(())
}
