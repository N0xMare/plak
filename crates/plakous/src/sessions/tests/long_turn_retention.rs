//! One long Working turn keeps its superseded checkpoints bounded: the owner
//! prunes between rounds, not only after the turn's terminal.

use super::*;
use plakous_agent::AgentConfig;
use plakous_model::{CallId, ToolCallBlock, ToolCallState, ToolCorrelation, ToolExecutionOwner};

const ROUNDS: usize = 200;
const WINDOW: usize = 16;
/// One automatic prune's minimum, the 64 checkpoints a Working-head prune
/// never removes, and what a few rounds commit before the next round boundary
/// reaches it.
const CHECKPOINT_BOUND: u64 = 160;

/// Store occupancy sampled as each round starts streaming.
#[derive(Clone, Copy)]
struct Sample {
    checkpoints: u64,
    bytes: u64,
}

/// Calls `probe` for `ROUNDS` rounds, then answers.
struct ToolLoopModel {
    scripted: Arc<plakous_test_support::ScriptedModel>,
    store: MemorySessionStore,
    calls: AtomicUsize,
    samples: Mutex<Vec<Sample>>,
}

impl LanguageModel for ToolLoopModel {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        self.scripted.clone().revalidate_runtime_authority(at)
    }

    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        let inert = self.scripted.clone().prepare_call(plan.clone(), context)?;
        Ok(PreparedModelCall::new(inert.record().clone(), move |_| {
            Box::pin(async move {
                let failure =
                    || ModelError::new(ModelErrorKind::InvalidEvent, "tool loop fixture failed");
                let ordinal = self.calls.fetch_add(1, Ordering::AcqRel);
                let stats = self.store.stats().map_err(|_| failure())?;
                test_lock(&self.samples).push(Sample {
                    checkpoints: u64::try_from(stats.checkpoints()).unwrap_or(u64::MAX),
                    bytes: u64::try_from(stats.unique_object_bytes()).unwrap_or(u64::MAX),
                });
                let id = Uuid::from_bytes(
                    plan.digest().bytes()[..16]
                        .try_into()
                        .map_err(|_| failure())?,
                );
                let (block, finish) = if ordinal < ROUNDS {
                    (
                        ContentBlock::ToolCall(
                            ToolCallBlock::new(
                                CallId::try_from(id).map_err(|_| failure())?,
                                ToolName::new("probe").map_err(|_| failure())?,
                                ToolExecutionOwner::Local,
                                ToolCallState::Requested,
                                ToolCorrelation::default(),
                                "{}",
                            )
                            .map_err(|_| failure())?,
                        ),
                        FinishReason::ToolCalls,
                    )
                } else {
                    (
                        ContentBlock::Text(TextBlock::new("done", None).map_err(|_| failure())?),
                        FinishReason::Stop,
                    )
                };
                let output = ConversationItem::new(
                    EntryId::try_from(id).map_err(|_| failure())?,
                    Actor::Assistant,
                    vec![block],
                    Provenance::ModelOutput,
                )
                .map_err(|_| failure())?;
                let response = ModelResponse::new(
                    vec![output.clone()],
                    FinishDetails::new(finish, None),
                    None,
                    None,
                    plan.digest(),
                );
                Ok(Box::pin(FixtureStream {
                    events: vec![
                        Ok(ModelEvent::ResponseStarted(ResponseStarted::new(
                            None,
                            plan.digest(),
                        ))),
                        Ok(ModelEvent::BlockStarted(BlockStarted::new(
                            output.id(),
                            OutputLocation::new(0, 0),
                            BlockStart::Complete(output.blocks()[0].clone()),
                        ))),
                        Ok(ModelEvent::ResponseCompleted(response)),
                    ]
                    .into(),
                }) as ModelStream)
            })
        }))
    }
}

fn tool_loop_binding(
    workspace: Arc<WorkspacePolicy>,
    model: Arc<ToolLoopModel>,
    authority: RuntimeModelAuthority,
) -> TestResult<SessionRuntimeBinding> {
    let descriptor = descriptor()?;
    let definition = one_tool_registry()?.definitions()[0].clone();
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("probe.registration")?,
        ToolExecutorId::new("probe.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"probe.executor"])?,
        definition,
        Arc::new(plakous_test_support::ScriptedTool::new((0..ROUNDS).map(
            |round| ToolOutcome::success_text(format!("{round:04} {}", "r".repeat(2_000))),
        ))),
    ))?;
    let registry = registry.freeze();
    let model: Arc<dyn LanguageModel> = model;
    let configured = crate::test_model::configured_model(
        model,
        descriptor,
        authority,
        registry.definitions().iter().cloned(),
        LocalDurabilityDisposition::Ephemeral,
        OutputBoundDisposition::Enforced(NonZeroU32::new(128).ok_or("output")?),
    )?;
    Ok(SessionRuntimeBinding::new(
        AgentBuilder::new(configured, registry, workspace)
            .request_policy(AgentRequestPolicy::new(
                GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                ReasoningPolicy::default(),
            ))
            .config(AgentConfig::default().with_max_model_rounds(ROUNDS + 1))
            .identity_sources(
                Arc::new(FixedClock::new(timestamp()?)),
                Arc::new(SystemIdGenerator),
            )
            .with_scheduled_compaction_enabled(false),
    ))
}

fn mean(samples: &[Sample], pick: fn(&Sample) -> u64) -> u64 {
    let total = samples.iter().map(pick).sum::<u64>();
    total / u64::try_from(samples.len().max(1)).unwrap_or(1)
}

#[tokio::test]
async fn a_long_working_turn_prunes_superseded_checkpoints_between_rounds() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = MemorySessionStore::with_clock(
        MemoryStoreLimits::default(),
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let descriptor = descriptor()?;
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"long-turn-account"])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"long-turn-credential"])?,
        1,
        timestamp()?,
        None,
    )?;
    let model = Arc::new(ToolLoopModel {
        scripted: Arc::new(
            plakous_test_support::ScriptedModel::new(descriptor, [])
                .with_runtime_authority(authority.clone())
                .with_context_observation(crate::test_model::context_observation()?),
        ),
        store: store.clone(),
        calls: AtomicUsize::new(0),
        samples: Mutex::new(Vec::new()),
    });
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Disabled,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("long-turn:create")?,
            tool_loop_binding(Arc::clone(&workspace), Arc::clone(&model), authority)?,
        ))
        .await?;
    let result = session
        .agent()
        .prompt("probe until done")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(result.model_rounds(), ROUNDS + 1);
    let samples = test_lock(&model.samples).clone();
    assert_eq!(samples.len(), ROUNDS + 1);
    let peak = samples
        .iter()
        .map(|sample| sample.checkpoints)
        .max()
        .unwrap_or(0);
    let (early, late) = (&samples[1..=WINDOW], &samples[ROUNDS + 1 - WINDOW..]);
    let growth = |window: &[Sample]| {
        let first = window.first().map_or(0, |sample| sample.bytes);
        let last = window.last().map_or(0, |sample| sample.bytes);
        last.saturating_sub(first)
            / u64::try_from(window.len().saturating_sub(1).max(1)).unwrap_or(1)
    };
    eprintln!(
        "long-turn retention: peak {peak} checkpoints in {ROUNDS} rounds; checkpoints at rounds 1-{WINDOW} mean {}, at {}-{} mean {}; retained bytes {} -> {} (mean growth {} B/round early, {} B/round late)",
        mean(early, |sample| sample.checkpoints),
        ROUNDS + 1 - WINDOW,
        ROUNDS,
        mean(late, |sample| sample.checkpoints),
        samples[1].bytes,
        samples[ROUNDS].bytes,
        growth(early),
        growth(late),
    );
    assert!(
        peak <= CHECKPOINT_BOUND,
        "the Working turn let {peak} checkpoints accumulate over {ROUNDS} rounds"
    );
    session.release().await?;
    Ok(())
}
