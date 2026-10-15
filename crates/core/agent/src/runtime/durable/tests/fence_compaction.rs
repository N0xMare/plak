use super::*;

fn compaction_workflow_count(checkpoint: &RuntimeCheckpoint) -> usize {
    checkpoint
        .manifest()
        .ledger()
        .operations()
        .iter()
        .filter(|record| matches!(record.kind(), DurableOperationKind::Compaction { .. }))
        .count()
}

async fn assert_scheduled_fence_compaction_chrome(
    events: &mut TurnEvents,
) -> TestResult<CompactionEventOutcome> {
    let mut started_workflow = None;
    let mut finished_outcome = None;
    while let Ok(event) = events.recv().await {
        match event.kind() {
            AgentEventKind::CompactionStarted {
                workflow_id,
                reason,
            } => {
                assert_eq!(*reason, CompactionEventReason::Scheduled);
                assert!(started_workflow.replace(*workflow_id).is_none());
            }
            AgentEventKind::CompactionFinished {
                workflow_id,
                outcome,
                ..
            } => {
                assert_eq!(Some(*workflow_id), started_workflow.take());
                finished_outcome = Some(*outcome);
                assert_eq!(
                    *outcome,
                    CompactionEventOutcome::Committed,
                    "fence-(b) must Compacted-commit on the live head before the next model round"
                );
            }
            AgentEventKind::TurnFailed { .. } => {
                return Err("scheduled fence compact must not TurnFailed".into());
            }
            AgentEventKind::TurnCompleted { .. } => break,
            _ => {}
        }
    }
    assert!(started_workflow.is_none(), "every start has a finish");
    Ok(finished_outcome.ok_or("fence-(b) must emit CompactionFinished")?)
}

fn scheduled_tool_group_fence_agent_with_context_tokens(
    workspace: Arc<WorkspacePolicy>,
    scheduled: bool,
    entry_base: u128,
    context_tokens: u64,
) -> TestResult<(Agent, Arc<ArtifactModel>)> {
    let (builder, model) = scheduled_tool_group_fence_builder(
        workspace,
        scheduled,
        entry_base,
        context_tokens,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    Ok((builder.build()?, model))
}

pub(super) fn scheduled_tool_group_fence_builder(
    workspace: Arc<WorkspacePolicy>,
    scheduled: bool,
    entry_base: u128,
    context_tokens: u64,
    durability: LocalDurabilityDisposition,
) -> TestResult<(AgentBuilder, Arc<ArtifactModel>)> {
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            4,
            entry_base,
            false,
        )
        .with_tool_call_on(2)
        // Prepare 3 is the third turn's admission measure; growth starts at
        // its request so only the tool-group fence sees it.
        .with_context_tokens_from_prepare(4, context_tokens),
    );
    let builder = scheduled_tool_group_fence_builder_with_model(
        workspace,
        scheduled,
        Arc::clone(&model),
        durability,
    )?;
    Ok((builder, model))
}

pub(super) fn scheduled_tool_group_fence_builder_with_model(
    workspace: Arc<WorkspacePolicy>,
    scheduled: bool,
    model: Arc<ArtifactModel>,
    durability: LocalDurabilityDisposition,
) -> TestResult<AgentBuilder> {
    let descriptor = model.descriptor.clone();
    let authority = model.authority.clone();
    let runtime_model: Arc<dyn LanguageModel> = model;
    let registry = one_tool_registry()?;
    let configured = configured_test_model(
        runtime_model,
        descriptor,
        authority,
        &registry,
        durability,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let mut builder = AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        );
    if scheduled {
        builder = builder.with_scheduled_compaction_enabled(true);
    }
    Ok(builder)
}

fn scheduled_tool_group_fence_agent(
    workspace: Arc<WorkspacePolicy>,
    scheduled: bool,
    entry_base: u128,
) -> TestResult<(Agent, Arc<ArtifactModel>)> {
    scheduled_tool_group_fence_agent_with_context_tokens(workspace, scheduled, entry_base, 900_000)
}

#[tokio::test]
async fn compact_session_from_endpoint_during_in_flight_model_is_busy() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let joined = Arc::new(AtomicBool::new(false));
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            9_550,
            false,
        )
        .with_joined_cancellation(Arc::clone(&joined)),
    );
    let agent = artifact_test_builder(workspace.clone(), Arc::clone(&model))
        .build()
        .map_err(|error| error.message().to_owned())?;
    let turn = agent.prompt("compact while busy").await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while model.calls.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let checkpoint = agent.checkpoint();
    let topology = checkpoint.manifest().topology();
    let (builder, _) = recovery_compaction_fixture(
        workspace,
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    let compact = tokio::time::timeout(
        Duration::from_millis(100),
        agent
            .session_endpoint()
            .compact_session(AgentCompactSessionRequest::new(
                "compact:busy-turn".to_owned(),
                topology.session_id,
                topology.branch_id,
                topology.checkpoint_id,
                AgentCompactionReason::Manual,
                builder,
            )),
    )
    .await?;
    assert_eq!(compact, Err(AgentSessionEndpointError::Busy));
    assert_eq!(
        turn.control().cancel().await?,
        CancellationStatus::Requested
    );
    let result = turn.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Cancelled(_)));
    assert!(joined.load(Ordering::Acquire));
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn scheduled_tool_group_fence_compacts_then_continues_turn() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (agent, model) = scheduled_tool_group_fence_agent(Arc::clone(&workspace), true, 9_600)?;
    for prompt in ["older replaceable intent", "latest protected intent"] {
        let result = agent.prompt(prompt).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    let mut turn = agent
        .prompt("use the growing probe at the tool-group fence")
        .await?;
    let mut events = turn.take_events()?;
    let result = turn.result().await?;
    let compact_outcome = assert_scheduled_fence_compaction_chrome(&mut events).await?;
    assert_eq!(compact_outcome, CompactionEventOutcome::Committed);
    assert!(
        compaction_workflow_count(result.checkpoint()) >= 1,
        "fence-(b) must persist a compaction workflow on the live head"
    );
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(result.tool_calls(), 1);
    assert!(
        model.calls.load(Ordering::Acquire) >= 3,
        "tool turn continued after the scheduled-compaction fence path"
    );
    assert!(matches!(
        result.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    // The fold reset the anchor mid-turn, so the turn terminal records the
    // anchor of a round sent after it, never the pre-compaction one.
    let anchor = result
        .checkpoint()
        .manifest()
        .canonical()
        .context()
        .occupancy_anchor()
        .ok_or("the fenced turn's terminal must record an anchor")?;
    // Rounds before the fence report 64; only the round sent after the fold
    // reports the fixture's grown count.
    assert_eq!(anchor.provider_input_tokens(), 900_002);
    let estimates = lock_mutex(&model.prepared_estimates).clone();
    let fold = estimates
        .iter()
        .rposition(|(compaction, _)| *compaction)
        .ok_or("the fence must have prepared a fold")?;
    let anchored = Some(anchor.request_estimate_tokens().saturating_sub(2));
    assert!(
        estimates[fold + 1..]
            .iter()
            .any(|(compaction, estimate)| !compaction && *estimate == anchored),
        "the terminal anchor must be a post-compaction request: {anchor:?} {estimates:?}"
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn scheduled_tool_group_fence_compacts_on_exhausted_reserve_below_occupancy_trigger()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    // The 128-token output cap leaves 5,072 input tokens. Before the tool
    // round the fixture counts 64; its post-tool projection counts 1,000:
    // still Fits, but only 4,072 tokens remain for the 4,096-token reserve.
    // Occupancy is deliberately below the capacity-derived trigger.
    let descriptor = placeholder_test_descriptor(5_200, false)?;
    let mut fixture = ArtifactModel::new(
        descriptor.clone(),
        artifact_authority(&descriptor, 1)?,
        4,
        9_605,
        false,
    )
    .with_tool_call_on(2)
    .with_context_tokens_from_prepare(4, 1_000);
    fixture.response_provenance = true;
    let model = Arc::new(fixture);
    let agent = scheduled_tool_group_fence_builder_with_model(
        workspace,
        true,
        Arc::clone(&model),
        LocalDurabilityDisposition::Ephemeral,
    )?
    .build()?;
    for prompt in ["older replaceable intent", "latest protected intent"] {
        let result = agent.prompt(prompt).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    assert_eq!(compaction_workflow_count(&agent.checkpoint()), 0);

    let mut turn = agent
        .prompt("use the probe with a narrow input envelope")
        .await?;
    let root = turn.id();
    let mut events = turn.take_events()?;
    let result = turn.result().await?;
    assert_eq!(
        assert_scheduled_fence_compaction_chrome(&mut events).await?,
        CompactionEventOutcome::Committed
    );
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(result.turn_id(), root);
    assert_eq!(result.model_rounds(), 2);
    assert_eq!(result.tool_calls(), 1);
    assert_eq!(compaction_workflow_count(result.checkpoint()), 1);
    // Two seed calls, the tool round, exactly one paid fold, then the
    // completing model round. No duplicate tool or provider dispatch.
    assert_eq!(model.calls.load(Ordering::Acquire), 5);

    {
        let estimates = lock_mutex(&model.prepared_estimates);
        let observed = lock_mutex(&model.observed_provenance);
        let records = lock_mutex(&model.prepared_records);
        let folds = estimates
            .iter()
            .enumerate()
            .filter_map(|(index, (compaction, _))| compaction.then_some(index))
            .collect::<Vec<_>>();
        assert_eq!(folds.len(), 1, "exactly one paid summary was prepared");
        let fold = folds[0];
        let post_tool = fold.checked_sub(1).ok_or("missing post-tool measure")?;
        let (plan, context) = &observed[post_tool];
        let status = descriptor
            .context_status(plan, records[post_tool].projection(), context.prepared_at())?
            .with_input_parts(
                records[post_tool]
                    .projection()
                    .token_projection()
                    .input_parts(),
            )
            .with_context_anchor(plakous_model::ContextAnchor::new(
                64,
                estimates[2].1.ok_or("missing tool-round estimate")?,
            ));
        assert_eq!(
            status.kind(),
            &plakous_model::ContextStatusKind::Fits {
                headroom: plakous_model::ContextHeadroom::Shared(
                    plakous_model::QualifiedTokens::Exact(4_072),
                ),
            }
        );
        assert_eq!(status.occupancy_tokens(), Some(1_000));
        assert!(status.occupancy_tokens().ok_or("missing occupancy")? < 4_420);
        assert!(!estimates[post_tool].0);
        assert!(
            fold + 1 < observed.len(),
            "a model request must follow the fold"
        );
        let tool_results = plan
            .request()
            .input()
            .items()
            .iter()
            .flat_map(ConversationItem::blocks)
            .filter_map(|block| match block {
                ContentBlock::ToolResult(tool) => Some(tool.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(tool_results.len(), 1);
        assert!(
            matches!(tool_results[0].parts(), [ToolResultPart::Text(text)] if text.text() == "ok")
        );
        // The paid fold must not replace the fresh observation with a
        // placeholder: the subsequent model still receives it byte-for-byte.
        let subsequent = &observed.last().ok_or("missing completing request")?.0;
        let retained = subsequent
            .request()
            .input()
            .items()
            .iter()
            .flat_map(ConversationItem::blocks)
            .filter_map(|block| match block {
                ContentBlock::ToolResult(tool) => Some(tool.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(retained, tool_results);
        assert!(subsequent.request().input().items().iter()
            .flat_map(ConversationItem::blocks)
            .any(|block| matches!(block, ContentBlock::Compaction(summary) if summary.summary() == "artifact response 3")),
            "the completing request must consume the paid fold's summary");
    }
    assert!(matches!(
        result.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    agent.shutdown().await?;
    Ok(())
}

/// The turn fences act on an anchor a reopen restored from the
/// checkpoint: the reopened model reports no usage, so the persisted anchor is
/// the only one the measure can have.
#[tokio::test]
async fn a_reopened_session_fences_on_its_persisted_anchor() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let anchoring = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            4,
            9_880,
            false,
        )
        // The second turn's request anchors the session after its own
        // admission measure.
        .with_context_tokens_from_prepare(2, 900_000),
    );
    let agent = sessions
        .create(
            "create:persisted-anchor-fence",
            scheduled_tool_group_fence_builder_with_model(
                Arc::clone(&workspace),
                true,
                anchoring,
                LocalDurabilityDisposition::DurableRequired,
            )?,
        )
        .await?;
    for prompt in ["older replaceable intent", "latest protected intent"] {
        let result = agent.prompt(prompt).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    let checkpoint = agent.checkpoint();
    let anchor = checkpoint
        .manifest()
        .canonical()
        .context()
        .occupancy_anchor()
        .ok_or("the quiescent checkpoint must record the anchor")?;
    assert_eq!(anchor.provider_input_tokens(), 900_002);
    let address =
        DurableSessionAddress::new(agent.session_id(), agent.branch_id(), checkpoint.id());
    agent.shutdown().await?;

    let silent = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            4,
            9_890,
            false,
        )
        // Call 0 is the admission fold on the persisted anchor.
        .with_tool_call_on(1),
    );
    let DurableRuntimeOpen::Opened(agent) = sessions
        .open(
            "open:persisted-anchor-fence",
            address,
            scheduled_tool_group_fence_builder_with_model(
                Arc::clone(&workspace),
                true,
                Arc::clone(&silent),
                LocalDurabilityDisposition::DurableRequired,
            )?,
        )
        .await?
    else {
        return Err("anchored session did not reopen".into());
    };
    let result = agent
        .prompt("use the probe on the reopened session")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(result.tool_calls(), 1);
    assert!(
        compaction_workflow_count(result.checkpoint()) >= 1,
        "the persisted anchor must let the reopened turn's fences compact"
    );
    assert!(
        lock_mutex(&silent.prepared_estimates)
            .iter()
            .any(|(compaction, _)| *compaction),
        "the fence compacted through the reopened model"
    );
    agent.shutdown().await?;
    Ok(())
}

pub(super) fn provider_overflow_agent(
    workspace: Arc<WorkspacePolicy>,
    entry_base: u128,
    overflow_calls: &[usize],
) -> TestResult<(Agent, Arc<ArtifactModel>)> {
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            4,
            entry_base,
            false,
        )
        .with_provider_overflows(overflow_calls.iter().copied()),
    );
    let authority = model.authority.clone();
    let runtime_model: Arc<dyn LanguageModel> = model.clone();
    let registry = one_tool_registry()?;
    let configured = configured_test_model(
        runtime_model,
        descriptor,
        authority,
        &registry,
        LocalDurabilityDisposition::Ephemeral,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let agent = AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        )
        .with_scheduled_compaction_enabled(true)
        // Stands in for the plan pack: a note that names how much history
        // the compaction consumed, so a test can tell which pass wrote it.
        .post_compaction_note(Arc::new(|source, successor| {
            (successor.segments().len() < source.segments().len()).then(|| {
                format!(
                    "restored plan after folding {} segments",
                    source.segments().len()
                )
            })
        }))
        .build()?;
    Ok((agent, model))
}

fn steering_notes(checkpoint: &RuntimeCheckpoint) -> Vec<String> {
    checkpoint
        .manifest()
        .canonical()
        .history()
        .segments()
        .iter()
        .filter(|segment| segment.kind() == HistorySegmentKind::Steering)
        .flat_map(HistorySegment::items)
        .flat_map(ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::Text(text) if text.text().starts_with("restored plan") => {
                Some(text.text().to_owned())
            }
            _ => None,
        })
        .collect()
}

fn user_items_with_text(checkpoint: &RuntimeCheckpoint, text: &str) -> usize {
    checkpoint
        .manifest()
        .canonical()
        .history()
        .segments()
        .iter()
        .flat_map(HistorySegment::items)
        .filter(|item| {
            item.actor() == Actor::User
                && item
                    .blocks()
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text(body) if body.text() == text))
        })
        .count()
}

/// A provider refusing a round's input as over its window is answered the
/// way the meter would have: compact, then send the round again as a new
/// operation. The prompt is history already and must not be appended twice.
#[tokio::test]
async fn a_provider_overflow_compacts_and_sends_the_round_again() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (agent, model) = provider_overflow_agent(Arc::clone(&workspace), 9_760, &[2])?;
    for prompt in ["older replaceable intent", "second older intent"] {
        let result = agent.prompt(prompt).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    let prompt = "answer after the provider refuses the window";
    let mut turn = agent.prompt(prompt).await?;
    let mut events = turn.take_events()?;
    let result = turn.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    let compact_outcome = assert_scheduled_fence_compaction_chrome(&mut events).await?;
    assert_eq!(compact_outcome, CompactionEventOutcome::Committed);
    assert_eq!(compaction_workflow_count(result.checkpoint()), 1);
    assert_eq!(user_items_with_text(result.checkpoint(), prompt), 1);
    // The note lands once, right after the commit, inside the same turn.
    assert_eq!(steering_notes(result.checkpoint()).len(), 1);
    // Two earlier turns, the refused round, the fold, and the resent round.
    assert_eq!(model.calls.load(Ordering::Acquire), 5);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn a_foreign_binding_overflow_does_not_spend_a_fold_or_resend() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (agent, model) = provider_overflow_agent(workspace, 9_790, &[2])?;
    model.foreign_overflow.store(true, Ordering::Release);
    for prompt in ["older replaceable intent", "second older intent"] {
        assert!(matches!(
            agent.prompt(prompt).await?.result().await?.outcome(),
            TurnResultOutcome::Completed(_)
        ));
    }
    let result = agent
        .prompt("current binding must ignore foreign overflow")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Failed(_)));
    assert_eq!(compaction_workflow_count(result.checkpoint()), 0);
    assert_eq!(model.calls.load(Ordering::Acquire), 3);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn item_admission_anchor_survives_a_big_session_reopen_without_forced_compaction()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let descriptor = placeholder_test_descriptor(1_000_000, false)?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            98_000,
            false,
        )
        .with_context_tokens_from_prepare(0, 298_468),
    );
    model.item_context.store(true, Ordering::Release);
    model
        .item_system_bytes
        .store(850_715 * 4, Ordering::Release);
    let builder = durable_artifact_test_builder(workspace.clone(), model.clone())
        .with_scheduled_compaction_enabled(true);
    let agent = sessions.create("create:item-anchor", builder).await?;
    let first = agent
        .prompt("first big session intent")
        .await?
        .result()
        .await?;
    assert!(
        matches!(first.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        first.outcome()
    );
    let checkpoint = agent.checkpoint();
    assert!(
        checkpoint
            .manifest()
            .canonical()
            .context()
            .request_anchor()
            .is_some()
    );
    let source = DurableSessionAddress::new(agent.session_id(), agent.branch_id(), checkpoint.id());
    agent.shutdown().await?;
    let outcome = sessions
        .open(
            "open:item-anchor",
            source,
            durable_artifact_test_builder(workspace, model.clone())
                .with_scheduled_compaction_enabled(true),
        )
        .await?;
    let DurableRuntimeOpen::Opened(agent) = outcome else {
        return Err("big session did not reopen".into());
    };
    let second = agent.prompt("small next intent").await?.result().await?;
    assert!(
        matches!(second.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        second.outcome()
    );
    assert_eq!(compaction_workflow_count(second.checkpoint()), 0);
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn near_ceiling_wire_with_one_third_provider_usage_preserves_or_compacts_observations()
-> TestResult {
    for (case, anchored, scheduled, fail_fold) in [
        ("anchored", true, true, false),
        ("unanchored_resume", false, true, false),
        ("disabled", false, false, false),
        ("failed_fold", false, true, true),
        ("clipped_failed_fold", false, true, true),
    ] {
        let clipped = case == "clipped_failed_fold";
        let body_bytes = if clipped { 100_000 } else { 25_000 };
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let descriptor = placeholder_test_descriptor(1_000_000, false)?;
        let model = Arc::new(
            ArtifactModel::new(
                descriptor.clone(),
                artifact_authority(&descriptor, 1)?,
                0,
                97_000,
                false,
            )
            .with_scripted_tool_calls([(1, "probe", "{}".to_owned())])
            .with_byte_derived_context_tokens(),
        );
        model.item_context.store(true, Ordering::Release);
        model
            .item_system_bytes
            .store(if clipped { 900_000 } else { 985_000 }, Ordering::Release);
        model
            .item_provider_input_tokens
            .store(if clipped { 980_000 } else { 336_000 }, Ordering::Release);
        model
            .usage_suppressed_until
            .store(usize::from(!anchored), Ordering::Release);
        model.script_tool_rounds_around_compactions();
        if fail_fold {
            model.fail_compaction_call();
        }
        let registry = oversized_tool_registry(&[body_bytes])?;
        let configured = configured_test_model(
            model.clone(),
            descriptor,
            model.authority.clone(),
            &registry,
            LocalDurabilityDisposition::DurableRequired,
            ContextAdmissionRequirement::AnyBounded,
        )?;
        let builder = || -> TestResult<AgentBuilder> {
            Ok(
                AgentBuilder::new(configured.clone(), registry.clone(), workspace.clone())
                    .request_policy(AgentRequestPolicy::new(
                        GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                        ReasoningPolicy::Disabled,
                    ))
                    .tool_policy(Arc::new(AlwaysAllowPolicy))
                    .identity_sources(
                        Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
                        Arc::new(SystemIdGenerator),
                    )
                    .with_scheduled_compaction_enabled(scheduled),
            )
        };
        let sessions = DurableSessionRuntime::current(
            Arc::new(TestCreateStore::default()),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("lease")?,
        )?;
        let agent = sessions
            .create("create:observation-pressure", builder()?)
            .await?;
        let first = agent
            .prompt("older intent ".repeat(150))
            .await?
            .result()
            .await?;
        assert!(
            matches!(first.outcome(), TurnResultOutcome::Completed(_)),
            "{case}: {:?}",
            first.outcome()
        );
        assert_eq!(
            first
                .checkpoint()
                .manifest()
                .canonical()
                .context()
                .request_anchor()
                .is_some(),
            anchored,
            "{case}"
        );
        let address = DurableSessionAddress::new(
            agent.session_id(),
            agent.branch_id(),
            agent.checkpoint().id(),
        );
        agent.shutdown().await?;
        let DurableRuntimeOpen::Opened(agent) = sessions
            .open("open:observation-pressure", address, builder()?)
            .await?
        else {
            return Err("observation-pressure session did not reopen".into());
        };
        let result = agent
            .prompt("read the 25 KB tool result")
            .await?
            .result()
            .await?;
        assert_eq!(result.tool_calls(), 1, "{case}: tools must never rerun");
        if clipped {
            assert!(
                matches!(result.outcome(), TurnResultOutcome::Completed(_)),
                "{case}: {:?}",
                result.outcome()
            );
            assert_eq!(
                model.calls.load(Ordering::Acquire),
                4,
                "seed, tool round, failed fold, and fitting continuation"
            );
            assert_eq!(result.model_rounds(), 2);
            agent.shutdown().await?;
            continue;
        }
        let checkpoint = result.checkpoint();
        let results = checkpoint
            .manifest()
            .canonical()
            .history()
            .segments()
            .iter()
            .flat_map(HistorySegment::items)
            .flat_map(ConversationItem::blocks)
            .filter_map(|block| match block {
                ContentBlock::ToolResult(result) => Some(result),
                _ => None,
            })
            .flat_map(ToolResultBlock::parts)
            .filter_map(|part| match part {
                ToolResultPart::Text(text) => Some(text.text()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            results.iter().any(|text| *text == "x".repeat(25_000)),
            "{case}: refreshed usage must preserve the output"
        );
        assert_eq!(compaction_workflow_count(checkpoint), 0, "{case}");
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{case}: {:?}",
            result.outcome()
        );
        assert!(
            model.observed_compaction_contexts().is_empty(),
            "{case}: no pressure after a fresh anchor"
        );
        agent.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn anchored_trigger_scale_paid_fold_keeps_the_multi_item_input() -> TestResult {
    for reserved in [false, true] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let descriptor = if reserved {
            custom_artifact_test_descriptor("test.route")?
        } else {
            placeholder_test_descriptor(1_000_000, false)?
        };
        let model = Arc::new(
            ArtifactModel::new(
                descriptor.clone(),
                artifact_authority(&descriptor, 1)?,
                0,
                100_000,
                false,
            )
            .with_scripted_tool_calls([(1, "probe", "{}".to_owned())])
            .with_byte_derived_context_tokens(),
        );
        model.item_context.store(true, Ordering::Release);
        model.item_system_bytes.store(2_700_000, Ordering::Release);
        model
            .item_provider_input_tokens
            .store(900_000, Ordering::Release);
        let registry = oversized_tool_registry(&[25_000])?;
        let configured = configured_test_model(
            model.clone(),
            descriptor,
            model.authority.clone(),
            &registry,
            LocalDurabilityDisposition::DurableRequired,
            ContextAdmissionRequirement::AnyBounded,
        )?;
        let builder = AgentBuilder::new(configured, registry, workspace)
            .request_policy(AgentRequestPolicy::new(
                GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                ReasoningPolicy::Disabled,
            ))
            .tool_policy(Arc::new(AlwaysAllowPolicy))
            .identity_sources(
                Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
                Arc::new(SystemIdGenerator),
            )
            .with_scheduled_compaction_enabled(true);
        let sessions = DurableSessionRuntime::current(
            Arc::new(TestCreateStore::default()),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("lease")?,
        )?;
        let agent = sessions.create("create:trigger-scale", builder).await?;
        assert!(matches!(
            agent
                .prompt("older eligible evidence")
                .await?
                .result()
                .await?
                .outcome(),
            TurnResultOutcome::Completed(_)
        ));
        let result = agent
            .prompt("read the retained evidence")
            .await?
            .result()
            .await?;
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{:?}",
            result.outcome()
        );
        assert_eq!(result.tool_calls(), 1);
        assert_eq!(compaction_workflow_count(result.checkpoint()), 1);
        assert!(
            model.observed_request_texts().iter().any(|items| items
                .iter()
                .any(|text| text == "older eligible evidence")
                && items
                    .iter()
                    .any(|text| text == plakous_model::SEMANTIC_COMPACTION_INSTRUCTION)),
            "fold must retain the large input, not elide it"
        );
        agent.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn recurring_oversized_tool_results_schedule_distinct_pressure_paid_folds() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        workspace,
        101_000,
        40_000,
        &[80_000; 4],
        &[1, 2, 3, 4],
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    model.script_tool_rounds_around_compactions();
    let agent = builder.build()?;
    assert!(matches!(
        agent
            .prompt("older eligible intent")
            .await?
            .result()
            .await?
            .outcome(),
        TurnResultOutcome::Completed(_)
    ));
    let result = agent
        .prompt("read oversized results repeatedly")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(result.tool_calls(), 4, "pressure must recur");
    // The fourth pressure fence is past the per-turn cap on committed
    // pressure-forced paid folds and uses the placeholder rung.
    assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 3);
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        result.model_rounds() + 4
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn a_pressure_fold_does_not_block_occupancy_or_provider_overflow_folds() -> TestResult {
    for provider_overflow in [true, false] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let descriptor = placeholder_test_descriptor(100_000, true)?;
        let model = Arc::new(
            ArtifactModel::new(
                descriptor.clone(),
                artifact_authority(&descriptor, 1)?,
                0,
                101_500,
                false,
            )
            .with_scripted_tool_calls(
                [1, 2]
                    .into_iter()
                    .take(if provider_overflow { 1 } else { 2 })
                    .map(|round| (round, "probe", "{}".to_owned())),
            )
            .with_byte_derived_context_tokens()
            .with_provider_overflows(provider_overflow.then_some(3)),
        );
        model.script_tool_rounds_around_compactions();
        model.item_context.store(true, Ordering::Release);
        let registry = oversized_tool_registry(&[250_000, 2_000])?;
        let configured = configured_test_model(
            model.clone(),
            descriptor,
            model.authority.clone(),
            &registry,
            LocalDurabilityDisposition::Ephemeral,
            ContextAdmissionRequirement::AnyBounded,
        )?;
        let agent = AgentBuilder::new(configured, registry, workspace)
            .request_policy(AgentRequestPolicy::new(
                GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                ReasoningPolicy::Disabled,
            ))
            .tool_policy(Arc::new(AlwaysAllowPolicy))
            .identity_sources(
                Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
                Arc::new(SystemIdGenerator),
            )
            .with_scheduled_compaction_enabled(true)
            .build()?;
        let seed = agent
            .prompt("older eligible intent")
            .await?
            .result()
            .await?;
        assert!(matches!(seed.outcome(), TurnResultOutcome::Completed(_)));
        // Near-full occupancy starts after the seed, so the next turn's
        // admission does not fold before its oversized read.
        model
            .item_provider_input_tokens
            .store(98_000, Ordering::Release);
        let result = agent
            .prompt("read oversized result")
            .await?
            .result()
            .await?;
        assert_eq!(result.tool_calls(), if provider_overflow { 1 } else { 2 });
        assert_eq!(
            model.calls.load(Ordering::Acquire),
            if provider_overflow { 5 } else { 6 },
            "the later provider refusal or occupancy fence must get its own paid fold: {:?}; contexts {}",
            result.outcome(),
            model.observed_compaction_contexts().len()
        );
        assert_eq!(
            model.observed_compaction_contexts().len(),
            2,
            "occupancy and provider-overflow folds must still dispatch after the pressure fold"
        );
        if !provider_overflow {
            assert!(
                matches!(result.outcome(), TurnResultOutcome::Completed(_)),
                "{:?}",
                result.outcome()
            );
        }
        agent.shutdown().await?;
    }
    Ok(())
}

/// A provider may report a served id other than the requested one (the Grok
/// subscription answers `grok-4.7` as `grok-4.7-build`). Only a
/// descriptor-declared same served model anchors the item-delta meter; an
/// undeclared id never does.
#[tokio::test]
async fn only_a_declared_same_served_model_anchors_the_meter() -> TestResult {
    for (declared, served, anchored) in [
        (true, "served-variant", true),
        (true, "undeclared-variant", false),
        (false, "served-variant", false),
    ] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let mut descriptor = placeholder_test_descriptor(1_000_000, false)?;
        if declared {
            descriptor = descriptor.with_same_served_models([ModelId::new("served-variant")?]);
        }
        let (builder, model) = placeholder_fixture_with_descriptor(
            workspace,
            99_400,
            descriptor,
            &[],
            &[],
            LocalDurabilityDisposition::Ephemeral,
            0,
            &[],
        )?;
        model.item_context.store(true, Ordering::Release);
        *lock_mutex(&model.served_model) = Some(ModelId::new(served)?);
        let agent = builder.build()?;
        let mut last = None;
        for prompt in ["first served intent", "second served intent"] {
            let result = agent.prompt(prompt).await?.result().await?;
            assert!(
                matches!(result.outcome(), TurnResultOutcome::Completed(_)),
                "{served}: {:?}",
                result.outcome()
            );
            assert_eq!(result.actual_model().map(ModelId::as_str), Some(served));
            last = Some(result);
        }
        let last = last.ok_or("no turn")?;
        let anchor = last
            .checkpoint()
            .manifest()
            .canonical()
            .context()
            .request_anchor();
        assert_eq!(anchor.is_some(), anchored, "{served} declared={declared}");
        assert_eq!(
            *lock_mutex(&model.observed_request_anchors),
            vec![false, anchored, anchored],
            "{served} declared={declared}"
        );
        agent.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn item_admission_handles_a_long_single_logical_tool_turn() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        workspace,
        99_000,
        100_000,
        &[2000; 24],
        &(0..24).collect::<Vec<_>>(),
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    model.item_context.store(true, Ordering::Release);
    let agent = builder.build()?;
    let result = agent
        .prompt("consume every requested tool group within this one logical turn")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 25);
    assert_eq!(compaction_workflow_count(result.checkpoint()), 0);
    assert!(
        result
            .checkpoint()
            .manifest()
            .canonical()
            .context()
            .request_anchor()
            .is_some()
    );
    agent.shutdown().await?;
    Ok(())
}

/// An idle compaction commits its note with the successor, so release and
/// reopen cannot lose it and the next prompt still follows it exactly once.
#[tokio::test]
async fn an_idle_compaction_note_opens_the_next_turn_once() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            9_800,
            false,
        )
        .with_report_usage(),
    );
    model.item_context.store(true, Ordering::Release);
    let agent = sessions
        .create(
            "create:idle-note",
            durable_artifact_test_builder(Arc::clone(&workspace), Arc::clone(&model))
                .post_compaction_note(Arc::new(|_, _| Some("restored plan: idle".to_owned()))),
        )
        .await?;
    for prompt in ["older replaceable intent", "latest protected intent"] {
        let result = agent.prompt(prompt).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    assert!(
        agent
            .checkpoint()
            .manifest()
            .canonical()
            .context()
            .request_anchor()
            .is_some()
    );
    let topology = agent.checkpoint().manifest().topology();
    let (builder, _) = recovery_compaction_fixture(
        Arc::clone(&workspace),
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            "compact:idle-note".to_owned(),
            topology.session_id,
            topology.branch_id,
            topology.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|_| "idle compaction rejected its exact source")?;
    assert!(matches!(
        outcome,
        AgentCompactSessionOutcome::Compacted { .. }
    ));
    assert_eq!(
        steering_notes(&agent.checkpoint()),
        vec!["restored plan: idle".to_owned()],
        "the compaction successor must durably hold its restoration note"
    );
    assert!(
        agent
            .checkpoint()
            .manifest()
            .canonical()
            .context()
            .request_anchor()
            .is_none()
    );
    assert!(
        agent
            .checkpoint()
            .manifest()
            .canonical()
            .context()
            .occupancy_anchor()
            .is_none()
    );
    let address = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    agent.shutdown().await?;
    let reopen_builder = durable_artifact_test_builder(Arc::clone(&workspace), Arc::clone(&model))
        .post_compaction_note(Arc::new(|_, _| Some("restored plan: idle".to_owned())));
    let DurableRuntimeOpen::Opened(agent) = sessions
        .open("open:idle-note", address, reopen_builder)
        .await?
    else {
        return Err("idle-compacted session did not reopen".into());
    };
    let status = agent
        .session_endpoint()
        .context_status(
            agent.session_id(),
            agent.branch_id(),
            agent.checkpoint().id(),
            Some("first prompt after compaction".to_owned()),
        )
        .await
        .map_err(|_| "reopened compaction status")?;
    assert_eq!(status.context_anchor(), None);
    assert!(status.wire_input().is_some());
    assert!(
        status.uses_unanchored_estimate() || status.local_input_upper_bound().is_some(),
        "reopened admission must be unanchored or byte-bounded"
    );
    let prepared_before_prompt = model.prepared_records().len();
    for prompt in [
        "first prompt after compaction",
        "second prompt after compaction",
    ] {
        let result = agent.prompt(prompt).await?.result().await?;
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{:?}",
            result.outcome()
        );
    }
    let records = model.prepared_records();
    assert!(
        records[prepared_before_prompt]
            .projection()
            .token_projection()
            .request_anchor()
            .is_none(),
        "the first post-reopen request must not restore the pre-fold anchor"
    );
    let checkpoint = agent.checkpoint();
    assert_eq!(
        steering_notes(&checkpoint),
        vec!["restored plan: idle".to_owned()]
    );
    let texts = checkpoint
        .manifest()
        .canonical()
        .history()
        .segments()
        .iter()
        .flat_map(HistorySegment::items)
        .filter(|item| item.actor() == Actor::User)
        .filter_map(|item| match item.blocks().first() {
            Some(ContentBlock::Text(text)) => Some(text.text().to_owned()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let note = texts.iter().position(|text| text == "restored plan: idle");
    let prompt = texts
        .iter()
        .position(|text| text == "first prompt after compaction");
    assert_eq!(note.map(|index| index + 1), prompt, "{texts:?}");
    agent.shutdown().await?;
    Ok(())
}

/// A plain reopen keeps the context-measure anchor the last turn terminal
/// recorded, so the reopened session's measure is the provider's count and
/// not the unanchored whole-request estimate; a committed compaction clears
/// it again.
#[tokio::test]
async fn a_context_anchor_survives_reopen_and_a_failed_compaction_and_a_fold_clears_it()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let descriptor = durable_test_descriptor("test.route")?;
    // The fixture refuses a second lowering of one attempt, and the status
    // request ids derive from the checkpoint, so the reopen gets its own model.
    let fresh_model = || -> TestResult<Arc<ArtifactModel>> {
        Ok(Arc::new(
            ArtifactModel::new(
                descriptor.clone(),
                artifact_authority(&descriptor, 1)?,
                0,
                9_850,
                false,
            )
            .with_report_usage(),
        ))
    };
    let model = fresh_model()?;
    let agent = sessions
        .create(
            "create:anchor-reopen",
            durable_artifact_test_builder(Arc::clone(&workspace), Arc::clone(&model)),
        )
        .await?;
    for prompt in ["older replaceable intent", "latest protected intent"] {
        let result = agent.prompt(prompt).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    let checkpoint = agent.checkpoint();
    let anchor = checkpoint
        .manifest()
        .canonical()
        .context()
        .occupancy_anchor()
        .ok_or("the quiescent checkpoint must record the anchor")?;
    assert_eq!(anchor.provider_input_tokens(), 5);
    let live = agent
        .session_endpoint()
        .context_status(agent.session_id(), agent.branch_id(), checkpoint.id(), None)
        .await
        .map_err(|_| "live context status")?;
    assert_eq!(live.context_anchor(), Some(anchor));
    let address =
        DurableSessionAddress::new(agent.session_id(), agent.branch_id(), checkpoint.id());
    agent.shutdown().await?;

    let DurableRuntimeOpen::Opened(agent) = sessions
        .open(
            "open:anchor-reopen",
            address,
            durable_artifact_test_builder(Arc::clone(&workspace), fresh_model()?),
        )
        .await?
    else {
        return Err("anchored session did not reopen".into());
    };
    let reopened = agent
        .session_endpoint()
        .context_status(
            agent.session_id(),
            agent.branch_id(),
            agent.checkpoint().id(),
            None,
        )
        .await
        .map_err(|_| "reopened context status")?;
    assert_eq!(reopened.context_anchor(), Some(anchor));
    assert_eq!(reopened.occupancy_tokens(), live.occupancy_tokens());

    // A compaction that fails definitely replaces nothing, so the measure
    // keeps its anchor in memory and in the checkpoint it publishes.
    let topology = agent.checkpoint().manifest().topology();
    let (failing, _) = recovery_compaction_fixture(
        Arc::clone(&workspace),
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::AmbiguousFailure(ModelError::new(
            ModelErrorKind::RateLimited,
            "provider rate limited the compaction",
        )),
        None,
    )?;
    let failed = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            "compact:anchor-failed".to_owned(),
            topology.session_id,
            topology.branch_id,
            topology.checkpoint_id,
            AgentCompactionReason::Manual,
            failing,
        ))
        .await
        .map_err(|_| "the failing compaction rejected its exact source")?;
    assert!(
        matches!(failed, AgentCompactSessionOutcome::Failed { .. }),
        "expected a failed compaction, got {failed:?}"
    );
    assert_eq!(
        agent
            .checkpoint()
            .manifest()
            .canonical()
            .context()
            .occupancy_anchor(),
        Some(anchor)
    );
    let after_failure = agent
        .session_endpoint()
        .context_status(
            agent.session_id(),
            agent.branch_id(),
            agent.checkpoint().id(),
            None,
        )
        .await
        .map_err(|_| "context status after the failed compaction")?;
    assert_eq!(after_failure.context_anchor(), Some(anchor));

    let topology = agent.checkpoint().manifest().topology();
    let (builder, _) = recovery_compaction_fixture(
        Arc::clone(&workspace),
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            "compact:anchor-reopen".to_owned(),
            topology.session_id,
            topology.branch_id,
            topology.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|_| "idle compaction rejected its exact source")?;
    let AgentCompactSessionOutcome::Compacted { summary, .. } = outcome else {
        return Err(format!("expected a committed compaction, got {outcome:?}").into());
    };
    let successor = agent.checkpoint();
    assert_eq!(
        successor
            .manifest()
            .canonical()
            .context()
            .occupancy_anchor(),
        None
    );
    let compacted = agent
        .session_endpoint()
        .context_status(agent.session_id(), agent.branch_id(), successor.id(), None)
        .await
        .map_err(|_| "successor context status")?;
    assert_eq!(compacted.context_anchor(), None);
    // The fold records both sides unanchored. The after side is the
    // compaction runtime's own successor measurement; this fixture's
    // compaction model is not the driver model, so only presence is pinned.
    let summary = summary.ok_or("committed fold summary")?;
    assert!(summary.occupancy_tokens_after.is_some());
    assert!(summary.occupancy_tokens_before.is_some());
    agent.shutdown().await?;
    Ok(())
}

/// One recovery per turn: a round refused again after the compaction fails
/// the turn definitely with the provider's classification.
#[tokio::test]
async fn a_second_provider_overflow_in_one_turn_fails_it() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (agent, model) = provider_overflow_agent(Arc::clone(&workspace), 9_780, &[2, 4])?;
    for prompt in ["older replaceable intent", "second older intent"] {
        let result = agent.prompt(prompt).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    let result = agent
        .prompt("still too long after compaction")
        .await?
        .result()
        .await?;
    let TurnResultOutcome::Failed(error) = result.outcome() else {
        return Err(format!("expected a definite failure, got {:?}", result.outcome()).into());
    };
    assert!(
        error
            .model_error()
            .is_some_and(ModelError::is_provider_context_overflow),
        "{error:?}"
    );
    assert_eq!(compaction_workflow_count(result.checkpoint()), 1);
    assert_eq!(model.calls.load(Ordering::Acquire), 5);
    assert!(matches!(
        result.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn scheduled_compaction_disabled_does_not_compact_at_tool_fence() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (agent, model) = scheduled_tool_group_fence_agent(Arc::clone(&workspace), false, 9_650)?;
    for prompt in ["older replaceable intent", "latest protected intent"] {
        let result = agent.prompt(prompt).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    let compactions_before = compaction_workflow_count(&agent.checkpoint());
    let result = agent
        .prompt("use the growing probe without scheduled compact")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(result.tool_calls(), 1);
    assert_eq!(
        compaction_workflow_count(result.checkpoint()),
        compactions_before,
        "disabled scheduled compaction must not start fence-(b) workflows"
    );
    assert!(model.calls.load(Ordering::Acquire) >= 3);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn scheduled_compaction_skips_when_occupancy_below_threshold() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (agent, model) = scheduled_tool_group_fence_agent_with_context_tokens(
        Arc::clone(&workspace),
        true,
        9_700,
        1_000,
    )?;
    for prompt in ["older replaceable intent", "latest protected intent"] {
        let result = agent.prompt(prompt).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    let compactions_before = compaction_workflow_count(&agent.checkpoint());
    let result = agent
        .prompt("use the growing probe below occupancy threshold")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(result.tool_calls(), 1);
    assert_eq!(
        compaction_workflow_count(result.checkpoint()),
        compactions_before,
        "Fits occupancy below the capacity-derived trigger must not start scheduled compaction"
    );
    assert!(model.calls.load(Ordering::Acquire) >= 3);
    assert!(matches!(
        result.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    agent.shutdown().await?;
    Ok(())
}
