use super::*;
use plakous_test_support::{ExpectedRequest, ModelScenario, ScenarioStart, ScenarioStep};

struct LastCallDecisionPolicy(Arc<AtomicUsize>);

impl ToolPolicy for LastCallDecisionPolicy {
    fn revision(&self) -> ToolPolicyRevision {
        ToolPolicyRevision::new("test.headroom-decision", 1).expect("fixed revision")
    }
    fn evaluate(&self, request: ToolPolicyRequest) -> ToolPolicyFuture {
        let decision = if request.invocation().call_id().as_uuid() == uuid::Uuid::from_u128(0x6017)
        {
            self.0.fetch_add(1, Ordering::AcqRel);
            ToolPolicyDecision::DecisionRequired(EffectDecisionPrompt::new(
                "last call requires approval",
            ))
        } else {
            ToolPolicyDecision::Allowed
        };
        Box::pin(async move { Ok(decision) })
    }
}

#[tokio::test]
async fn top_level_headroom_refusal_is_definite_source_ordered_and_prejournal() -> TestResult {
    if std::env::var_os("PLAK_TEST_HEADROOM_CHILD").is_none() {
        let name = concat!(
            module_path!(),
            "::top_level_headroom_refusal_is_definite_source_ordered_and_prejournal"
        )
        .strip_prefix("plakous_agent::")
        .ok_or("test name")?;
        for (parallel, decision) in [("0", "0"), ("1", "0"), ("1", "1")] {
            let output = std::process::Command::new(std::env::current_exe()?)
                .args(["--exact", name, "--nocapture"])
                .env("PLAK_TEST_HEADROOM_CHILD", "1")
                .env("PLAK_TEST_LEDGER_HEADROOM_LIMIT", "32")
                .env("PLAK_TEST_HEADROOM_PARALLEL", parallel)
                .env("PLAK_TEST_HEADROOM_LAST_DECISION", decision)
                .output()?;
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return Ok(());
    }
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = batch_model(&descriptor, &authority, [24])?;
    let registry = headroom_registry()?;
    let configured = configured_test_model(
        model.clone(),
        descriptor,
        authority,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let store = Arc::new(TestCreateStore::default());
    let decisions = Arc::new(AtomicUsize::new(0));
    let policy: Arc<dyn ToolPolicy> =
        if std::env::var("PLAK_TEST_HEADROOM_LAST_DECISION").as_deref() == Ok("1") {
            Arc::new(LastCallDecisionPolicy(decisions.clone()))
        } else {
            Arc::new(AlwaysAllowPolicy)
        };
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:headroom",
            AgentBuilder::new(configured, registry, workspace)
                .tool_policy(policy)
                .request_policy(AgentRequestPolicy::new(
                    GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                    ReasoningPolicy::Disabled,
                ))
                .identity_sources(
                    Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
                    Arc::new(SystemIdGenerator),
                ),
        )
        .await?;
    let result = agent.prompt("call the whole batch").await?.result().await?;
    assert_headroom_result(&result, &store)?;
    assert_eq!(
        decisions.load(Ordering::Acquire),
        0,
        "a refusal must not be followed by a decision barrier"
    );
    assert_eq!(model.captured_plans().len(), 2);
    agent.shutdown().await?;
    Ok(())
}

fn headroom_registry() -> TestResult<FrozenToolRegistry> {
    if std::env::var("PLAK_TEST_HEADROOM_PARALLEL").as_deref() != Ok("1") {
        return Ok(session_update_registry(SessionUpdateFixtureMode::Active)?.0);
    }
    probe_registry(
        Arc::new(CountingProbe::default()),
        ToolConcurrency::Parallel,
        ToolEffect::ReadOnly,
    )
}

#[derive(Default)]
struct CountingProbe(AtomicUsize);

impl plakous_tools::ToolExecutor for CountingProbe {
    fn execute(
        self: Arc<Self>,
        _invocation: ToolInvocation,
        _context: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        self.0.fetch_add(1, Ordering::AcqRel);
        Box::pin(async { ToolOutcome::success_text("ok") })
    }
}

fn probe_registry(
    probe: Arc<dyn plakous_tools::ToolExecutor>,
    concurrency: ToolConcurrency,
    effect: ToolEffect,
) -> TestResult<FrozenToolRegistry> {
    let definition = ToolDefinition::new(
        ToolName::new("probe")?,
        "1.0.0",
        "Parallel headroom probe.",
        ToolInputContract::structured(
            json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{},"required":[],"additionalProperties":false}),
        )?,
        ToolOutputContract::model_content(128, 1_048_576)?,
        concurrency,
        effect,
    )?;
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("probe.registration")?,
        ToolExecutorId::new("probe.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"probe.executor"])?,
        definition,
        probe,
    ))?;
    Ok(registry.freeze())
}

fn batch_model(
    descriptor: &ResolvedModelDescriptor,
    authority: &RuntimeModelAuthority,
    batches: impl IntoIterator<Item = usize>,
) -> TestResult<Arc<plakous_test_support::ScriptedModel>> {
    batch_model_for_calls(
        descriptor,
        authority,
        batches
            .into_iter()
            .map(|count| vec![("probe".to_owned(), "{}".to_owned()); count]),
    )
}

pub(super) fn batch_model_for_calls(
    descriptor: &ResolvedModelDescriptor,
    authority: &RuntimeModelAuthority,
    batches: impl IntoIterator<Item = Vec<(String, String)>>,
) -> TestResult<Arc<plakous_test_support::ScriptedModel>> {
    let digest = Digest::sha256(DigestDomain::RequestPlan, 1, &[b"headroom batch"])?;
    let response = |index, blocks, finish| -> TestResult<ModelScenario> {
        let item = ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(index))?,
            Actor::Assistant,
            blocks,
            Provenance::ModelOutput,
        )?;
        Ok(ModelScenario::new(
            ExpectedRequest::Any,
            ScenarioStart::Immediate,
            vec![
                ScenarioStep::Emit(ModelEvent::ResponseStarted(ResponseStarted::new(
                    None, digest,
                ))),
                ScenarioStep::Emit(ModelEvent::ResponseCompleted(ModelResponse::new(
                    vec![item],
                    FinishDetails::new(finish, None),
                    None,
                    None,
                    digest,
                ))),
            ],
        ))
    };
    let mut scenarios = Vec::new();
    let mut ordinal = 0;
    for (round, batch) in batches.into_iter().enumerate() {
        let per_batch = batch.len();
        let calls = batch
            .into_iter()
            .enumerate()
            .map(|(index, (name, arguments))| {
                Ok(ContentBlock::ToolCall(ToolCallBlock::new(
                    CallId::try_from(uuid::Uuid::from_u128(
                        0x6000 + u128::try_from(ordinal + index)?,
                    ))?,
                    ToolName::new(name)?,
                    ToolExecutionOwner::Local,
                    ToolCallState::Requested,
                    ToolCorrelation::default(),
                    arguments,
                )?))
            })
            .collect::<TestResult<Vec<_>>>()?;
        scenarios.push(response(
            0x7000 + u128::try_from(round)?,
            calls,
            FinishReason::ToolCalls,
        )?);
        ordinal += per_batch;
    }
    scenarios.push(response(
        0x7000 + u128::try_from(scenarios.len())?,
        vec![ContentBlock::Text(TextBlock::new("done", None)?)],
        FinishReason::Stop,
    )?);
    Ok(Arc::new(
        plakous_test_support::ScriptedModel::new(descriptor.clone(), scenarios)
            .with_runtime_authority(authority.clone())
            .with_context_observation(CountObservation::new(
                "agent-scripted-counter",
                "agent-scripted-exact-v1",
                64,
            )?),
    ))
}

fn assert_headroom_result(result: &TurnResult, store: &TestCreateStore) -> TestResult {
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert!(result.recovery_operation().is_none());
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
    assert_eq!(results.len(), 24);
    let effects = lock_mutex(&store.accepted_commits)
        .iter()
        .flat_map(|(checkpoint, _)| {
            checkpoint
                .manifest()
                .ledger()
                .operations()
                .into_iter()
                .filter_map(|record| match record.kind() {
                    DurableOperationKind::Effect { call_id, .. } => Some(*call_id),
                    _ => None,
                })
        })
        .collect::<BTreeSet<_>>();
    assert!(!effects.is_empty() && effects.len() < results.len());
    assert_eq!(
        result.usage().local_tool_attempts(),
        u64::try_from(effects.len())?
    );
    for (index, result) in results.iter().enumerate() {
        assert_eq!(
            result.call_id(),
            CallId::try_from(uuid::Uuid::from_u128(0x6000 + u128::try_from(index)?))?
        );
        if effects.contains(&result.call_id()) {
            continue;
        }
        assert_eq!(result.kind(), ToolResultKind::Failure);
        assert!(result.parts().iter().any(|part| matches!(part, ToolResultPart::Text(text) if text.text().contains(crate::runtime::operation_record::BATCH_HEADROOM_REFUSAL))));
    }
    Ok(())
}

#[tokio::test]
async fn ledger_retirement_5000_direct_batch_calls() -> TestResult {
    direct_batch_case(100, 50, None).await?;
    Ok(())
}

enum DirectBatchFault {
    AckLoss(usize),
    BeforeCas(SessionStoreError),
}

async fn direct_batch_case(
    batches: usize,
    per_batch: usize,
    fault: Option<DirectBatchFault>,
) -> TestResult<Arc<TestCreateStore>> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = batch_model(
        &descriptor,
        &authority,
        std::iter::repeat_n(per_batch, batches),
    )?;
    let probe = Arc::new(CountingProbe::default());
    let registry = probe_registry(
        probe.clone(),
        ToolConcurrency::Parallel,
        ToolEffect::ReadOnly,
    )?;
    let configured = configured_test_model(
        model.clone(),
        descriptor,
        authority,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let store = Arc::new(TestCreateStore::default());
    store.simulate_prune.store(true, Ordering::Release);
    let rejected = matches!(fault, Some(DirectBatchFault::BeforeCas(_)));
    match fault {
        Some(DirectBatchFault::AckLoss(call)) => store.lose_commit_ack_on(call),
        Some(DirectBatchFault::BeforeCas(error)) => {
            *lock_mutex(&store.reject_retirement_commit) = Some(error);
        }
        None => {}
    }
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:long-direct-batches",
            AgentBuilder::new(configured, registry, workspace)
                .config(AgentConfig::default().with_max_model_rounds(1_024))
                .tool_policy(Arc::new(AlwaysAllowPolicy))
                .request_policy(AgentRequestPolicy::new(
                    GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                    ReasoningPolicy::Disabled,
                ))
                .identity_sources(
                    Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
                    Arc::new(SystemIdGenerator),
                ),
        )
        .await?;
    let result = agent
        .prompt("complete every direct batch")
        .await?
        .result()
        .await;
    if rejected {
        assert!(
            result.is_err()
                || result.as_ref().is_ok_and(|result| !matches!(
                    result.outcome(),
                    TurnResultOutcome::Completed(_)
                ))
        );
        assert_refused_retirement(&store, &probe, &agent.checkpoint())?;
        let _ = agent.shutdown().await;
        return Ok(store);
    }
    let result = result?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert!(result.recovery_operation().is_none());
    assert_eq!(probe.0.load(Ordering::Acquire), batches * per_batch);
    assert_eq!(
        result.usage().local_tool_attempts(),
        u64::try_from(batches * per_batch)?
    );
    assert_eq!(
        result.usage().model_operations(),
        u64::try_from(batches + 1)?
    );
    assert_eq!(model.captured_plans().len(), batches + 1);
    let checkpoint = agent.checkpoint();
    let ledger = checkpoint.manifest().ledger();
    assert!(ledger.retired_attempts(result.turn_id()) > 0);
    assert!(
        lock_mutex(&store.accepted_commits)
            .iter()
            .all(|(checkpoint, _)| { checkpoint.manifest().ledger().operation_count() < 2_104 })
    );
    agent.shutdown().await?;
    Ok(store)
}

fn assert_refused_retirement(
    store: &TestCreateStore,
    probe: &CountingProbe,
    installed: &RuntimeCheckpoint,
) -> TestResult {
    let candidate = lock_mutex(&store.refused_retirement)
        .clone()
        .ok_or("retirement was not refused")?;
    let ledger = candidate.manifest().ledger();
    let (root, count) = ledger.retired_counter().ok_or("candidate counter")?;
    let retained = ledger.operations().into_iter().filter(|record| matches!(record.kind(), DurableOperationKind::Effect { turn_id, .. } if *turn_id == root) && record.transitions().iter().any(|transition| matches!(transition.state(), DurableOperationState::Dispatched))).count();
    assert_eq!(
        u64::try_from(probe.0.load(Ordering::Acquire))?,
        count + u64::try_from(retained)?
    );
    assert!(installed.manifest().ledger().retired_counter().is_none());
    assert!(
        lock_mutex(&store.accepted_commits)
            .iter()
            .all(|(checkpoint, _)| checkpoint.manifest().ledger().retired_counter().is_none())
    );
    let state = lock_mutex(&store.state);
    let stored = state
        .stored
        .as_ref()
        .ok_or("old head")?
        .checkpoint()
        .clone();
    assert_ne!(stored.id(), candidate.id());
    assert!(
        RuntimeCheckpointCodec::bundle(&stored, None)?
            .validated_checkpoint()?
            .manifest()
            .ledger()
            .retired_counter()
            .is_none()
    );
    let bundle = RuntimeCheckpointCodec::bundle(&candidate, None)?;
    let old = RuntimeCheckpointCodec::bundle(&stored, None)?;
    assert!(bundle.objects().iter().any(|object| {
        state.persisted_objects.contains(&object.address())
            && !old
                .objects()
                .iter()
                .any(|prior| prior.address() == object.address())
    }));
    Ok(())
}

#[tokio::test]
async fn retirement_objects_before_rejected_or_fenced_cas_do_not_install_authority() -> TestResult {
    if std::env::var_os("PLAK_TEST_LEDGER_REJECT_CHILD").is_none() {
        let name = concat!(
            module_path!(),
            "::retirement_objects_before_rejected_or_fenced_cas_do_not_install_authority"
        )
        .strip_prefix("plakous_agent::")
        .ok_or("test name")?;
        let output = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", name, "--nocapture"])
            .env("PLAK_TEST_LEDGER_REJECT_CHILD", "1")
            .env("PLAK_TEST_LEDGER_RETIREMENT_HIGH_WATER", "8")
            .output()?;
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return Ok(());
    }
    for error in [
        SessionStoreError::Conflict,
        SessionStoreError::FenceSuperseded,
    ] {
        direct_batch_case(8, 1, Some(DirectBatchFault::BeforeCas(error))).await?;
    }
    Ok(())
}

#[tokio::test]
async fn retirement_commit_ack_loss_recovers_the_counter_once() -> TestResult {
    if std::env::var_os("PLAK_TEST_LEDGER_ACK_CHILD").is_none() {
        let name = concat!(
            module_path!(),
            "::retirement_commit_ack_loss_recovers_the_counter_once"
        )
        .strip_prefix("plakous_agent::")
        .ok_or("test name")?;
        let output = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", name, "--nocapture"])
            .env("PLAK_TEST_LEDGER_ACK_CHILD", "1")
            .env("PLAK_TEST_LEDGER_RETIREMENT_HIGH_WATER", "8")
            .output()?;
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return Ok(());
    }
    let pilot = direct_batch_case(8, 1, None).await?;
    let counters = |store: &TestCreateStore| {
        lock_mutex(&store.accepted_commits)
            .iter()
            .map(|(checkpoint, _)| {
                checkpoint
                    .manifest()
                    .ledger()
                    .retired_counter()
                    .map_or(0, |(_, count)| count)
            })
            .collect::<Vec<_>>()
    };
    let before = counters(&pilot);
    let retirement = before
        .iter()
        .position(|count| *count > 0)
        .ok_or("retirement commit")?;
    let recovered =
        direct_batch_case(8, 1, Some(DirectBatchFault::AckLoss(retirement + 1))).await?;
    assert_eq!(recovered.lost_commit_acks.load(Ordering::Acquire), 1);
    assert!(recovered.recover_commit_calls.load(Ordering::Acquire) > 0);
    assert_eq!(counters(&recovered), before);
    let checkpoint = lock_mutex(&recovered.state)
        .stored
        .as_ref()
        .ok_or("head")?
        .checkpoint()
        .clone();
    let cold = RuntimeCheckpointCodec::bundle(&checkpoint, None)?.validated_checkpoint()?;
    assert_eq!(
        cold.manifest().ledger().retired_counter(),
        checkpoint.manifest().ledger().retired_counter()
    );
    Ok(())
}

#[derive(Default)]
struct CancellingProbe(AtomicUsize);

impl plakous_tools::ToolExecutor for CancellingProbe {
    fn execute(
        self: Arc<Self>,
        _invocation: ToolInvocation,
        context: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        let ordinal = self.0.fetch_add(1, Ordering::AcqRel);
        Box::pin(async move {
            if ordinal == 7 {
                context.cancellation().cancelled().await;
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Cancelled,
                    "interrupted batch fixture cancelled",
                ));
            }
            ToolOutcome::success_text("ok")
        })
    }
}

#[tokio::test]
async fn interrupted_batch_after_retirement_closes_exact_slots_and_usage() -> TestResult {
    if std::env::var_os("PLAK_TEST_LEDGER_INTERRUPT_CHILD").is_none() {
        let name = concat!(
            module_path!(),
            "::interrupted_batch_after_retirement_closes_exact_slots_and_usage"
        )
        .strip_prefix("plakous_agent::")
        .ok_or("test name")?;
        let output = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", name, "--nocapture"])
            .env("PLAK_TEST_LEDGER_INTERRUPT_CHILD", "1")
            .env("PLAK_TEST_LEDGER_RETIREMENT_HIGH_WATER", "8")
            .output()?;
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return Ok(());
    }
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = batch_model(
        &descriptor,
        &authority,
        std::iter::repeat_n(1, 6).chain([2]),
    )?;
    let probe = Arc::new(CancellingProbe::default());
    let registry = probe_registry(probe.clone(), ToolConcurrency::Serial, ToolEffect::Mutating)?;
    let configured = configured_test_model(
        model.clone(),
        descriptor,
        authority,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:interrupted-retired-batch",
            AgentBuilder::new(configured, registry, workspace)
                .tool_policy(Arc::new(AlwaysAllowPolicy))
                .request_policy(AgentRequestPolicy::new(
                    GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                    ReasoningPolicy::Disabled,
                ))
                .identity_sources(
                    Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
                    Arc::new(SystemIdGenerator),
                ),
        )
        .await?;
    let turn = agent.prompt("interrupt only the last batch").await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if probe.0.load(Ordering::Acquire) == 8 && agent.checkpoint().manifest().ledger().operations().iter().any(|record| {
                matches!(record.kind(), DurableOperationKind::Effect { call_id, .. } if *call_id == CallId::try_from(uuid::Uuid::from_u128(0x6006)).expect("call"))
                    && matches!(record.state(), DurableOperationState::Completed(_))
            }) { break; }
            tokio::task::yield_now().await;
        }
    }).await?;
    turn.control().cancel().await?;
    let result = turn.result().await?;
    assert_interrupted_retired_batch(&result, &agent.checkpoint())?;
    assert_eq!(probe.0.load(Ordering::Acquire), 8);
    assert_eq!(model.captured_plans().len(), 7);
    agent.shutdown().await?;
    Ok(())
}

fn assert_interrupted_retired_batch(
    result: &TurnResult,
    checkpoint: &RuntimeCheckpoint,
) -> TestResult {
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Cancelled(error) if error.kind() == AgentErrorKind::Cancelled)
    );
    assert!(result.recovery_operation().is_none());
    assert_eq!(result.usage().local_tool_attempts(), 8);
    let ledger = checkpoint.manifest().ledger();
    assert!(ledger.retired_attempts(result.turn_id()) > 0);
    for (index, kind) in [(0, ToolResultKind::Success), (1, ToolResultKind::Failure)] {
        let call = CallId::try_from(uuid::Uuid::from_u128(0x6006 + u128::from(index)))?;
        let results = result.conversation().items().iter().flat_map(ConversationItem::blocks).filter(|block| matches!(block, ContentBlock::ToolResult(result) if result.call_id() == call && result.kind() == kind)).count();
        assert_eq!(results, 1);
        let effect = ledger.operations().into_iter().find(|record| matches!(record.kind(), DurableOperationKind::Effect { call_id, source_index, .. } if *call_id == call && *source_index == index)).ok_or("exact source slot")?;
        assert!(matches!(
            effect.state(),
            DurableOperationState::Finalized(_)
        ));
    }
    Ok(())
}
