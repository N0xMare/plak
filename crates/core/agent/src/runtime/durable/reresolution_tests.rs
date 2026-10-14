//! A model past its admitted evidence window continues through a fresh exact
//! resolution swapped in between model operations (spec 01 §7.4).

use super::*;
use std::collections::VecDeque;

use plakous_model::Clock;

use crate::ModelRouteBinding;

use crate::runtime::{
    ModelReresolutionError, ModelReresolutionOperation, ModelReresolver, ReresolvedModel,
};

const WINDOW_END: u64 = 20;
const PAST_WINDOW: u64 = 30;

struct SettableClock(AtomicU64);

impl SettableClock {
    fn at(seconds: u64) -> Arc<Self> {
        Arc::new(Self(AtomicU64::new(seconds)))
    }

    fn set(&self, seconds: u64) {
        self.0.store(seconds, Ordering::Release);
    }
}

impl Clock for SettableClock {
    fn now(&self) -> Timestamp {
        Timestamp::new(self.0.load(Ordering::Acquire), 0).expect("test clock is in range")
    }
}

/// A model admitted over `[from, until)` whose descriptor was observed at
/// `observed`, on the artifact fixture's account unless `account` differs.
fn windowed_model(
    observed: u64,
    from: u64,
    until: u64,
    generation: u64,
    account: &[u8],
    entry_base: u128,
) -> TestResult<ArtifactModel> {
    let descriptor = durable_test_descriptor_at("test.route", Timestamp::new(observed, 0)?)?;
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[account])?,
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"artifact-credential-authority"],
        )?,
        generation,
        Timestamp::new(from, 0)?,
        Some(Timestamp::new(until, 0)?),
    )?;
    Ok(ArtifactModel::new(
        descriptor, authority, 0, entry_base, false,
    ))
}

fn configured(
    model: &Arc<ArtifactModel>,
    registry: &FrozenToolRegistry,
    durability: LocalDurabilityDisposition,
) -> TestResult<ConfiguredLanguageModel> {
    let runtime_model: Arc<dyn LanguageModel> = model.clone();
    configured_test_model(
        runtime_model,
        model.descriptor.clone(),
        model.authority.clone(),
        registry,
        durability,
        ContextAdmissionRequirement::AnyBounded,
    )
}

/// Hands out one prepared answer per call and counts calls.
struct ScriptedReresolver {
    answers: Mutex<VecDeque<Result<ReresolvedModel, ModelReresolutionError>>>,
    calls: AtomicUsize,
    accepted: Mutex<Vec<Timestamp>>,
}

impl ScriptedReresolver {
    fn new(
        answers: impl IntoIterator<Item = Result<ReresolvedModel, ModelReresolutionError>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.into_iter().collect()),
            calls: AtomicUsize::new(0),
            accepted: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }

    fn accepted_windows(&self) -> Vec<u64> {
        lock_mutex(&self.accepted)
            .iter()
            .map(|until| until.seconds())
            .collect()
    }
}

impl ModelReresolver for ScriptedReresolver {
    fn reresolve(self: Arc<Self>, expired_until: Timestamp) -> ModelReresolutionOperation {
        self.calls.fetch_add(1, Ordering::AcqRel);
        let answer = lock_mutex(&self.answers).pop_front().unwrap_or_else(|| {
            Err(ModelReresolutionError::new(
                "the scripted reresolver was asked more often than scripted",
            ))
        });
        Box::pin(async move {
            assert!(expired_until.seconds() > 0);
            answer
        })
    }

    fn accepted(&self, admitted_until: Timestamp) {
        lock_mutex(&self.accepted).push(admitted_until);
    }
}

fn reresolved(
    model: &Arc<ArtifactModel>,
    registry: &FrozenToolRegistry,
    durability: LocalDurabilityDisposition,
    until: u64,
) -> TestResult<ReresolvedModel> {
    Ok(ReresolvedModel::new(
        configured(model, registry, durability)?,
        Timestamp::new(until, 0)?,
    ))
}

fn durable_builder(
    workspace: &Arc<WorkspacePolicy>,
    configured: ConfiguredLanguageModel,
    registry: FrozenToolRegistry,
    clock: Arc<SettableClock>,
    telemetry: Option<Arc<TelemetryEmitter>>,
) -> TestResult<AgentBuilder> {
    let mut builder = AgentBuilder::new(configured, registry, Arc::clone(workspace))
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .identity_sources(clock, Arc::new(SystemIdGenerator));
    if let Some(telemetry) = telemetry {
        builder = builder.telemetry(telemetry);
    }
    Ok(builder)
}

fn hold_first_call(model: &ArtifactModel) -> Arc<Notify> {
    let release = Arc::new(Notify::new());
    model.held_call.store(0, Ordering::Release);
    *lock_mutex(&model.held_call_release) = Some(Arc::clone(&release));
    release
}

fn reresolutions(sink: &RecordingTelemetrySink) -> Vec<bool> {
    lock_mutex(&sink.observations)
        .iter()
        .filter_map(|observation| match observation.kind() {
            TelemetryObservationKind::ModelReresolved { accepted } => Some(accepted),
            _ => None,
        })
        .collect()
}

struct Durable {
    _directory: tempfile::TempDir,
    workspace: Arc<WorkspacePolicy>,
    sessions: DurableSessionRuntime,
    clock: Arc<SettableClock>,
    sink: Arc<RecordingTelemetrySink>,
}

impl Durable {
    fn new() -> TestResult<Self> {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let sessions = DurableSessionRuntime::current(
            Arc::new(TestCreateStore::default()),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("nonzero lease")?,
        )?;
        Ok(Self {
            _directory: directory,
            workspace,
            sessions,
            clock: SettableClock::at(10),
            sink: Arc::new(RecordingTelemetrySink {
                observations: Mutex::new(Vec::new()),
            }),
        })
    }

    fn builder(
        &self,
        model: &Arc<ArtifactModel>,
        reresolver: Option<Arc<ScriptedReresolver>>,
    ) -> TestResult<AgentBuilder> {
        let registry = one_tool_registry()?;
        let builder = durable_builder(
            &self.workspace,
            configured(
                model,
                &registry,
                LocalDurabilityDisposition::DurableRequired,
            )?,
            registry,
            Arc::clone(&self.clock),
            Some(Arc::new(TelemetryEmitter::enabled(self.sink.clone()))),
        )?;
        Ok(match reresolver {
            Some(reresolver) => {
                builder.model_reresolver(reresolver, Timestamp::new(WINDOW_END, 0)?)
            }
            None => builder,
        })
    }

    /// Reopens the closed session exactly with a fresh resolution, as the
    /// next process would, and returns the reopened binding.
    async fn reopen_exactly(
        &self,
        label: &str,
        agent: &Agent,
        fresh: &Arc<ArtifactModel>,
    ) -> TestResult<ModelRouteBinding> {
        let address = DurableSessionAddress::new(
            agent.session_id(),
            agent.branch_id(),
            agent.checkpoint().id(),
        );
        let reopened = self
            .sessions
            .open(label, address, self.builder(fresh, None)?)
            .await?;
        let DurableRuntimeOpen::Opened(reopened) = reopened else {
            return Err("the session did not reopen exactly after the swap".into());
        };
        let binding = reopened.checkpoint().manifest().binding().clone();
        reopened.shutdown().await?;
        Ok(binding)
    }
}

fn fresh_model(entry_base: u128) -> TestResult<Arc<ArtifactModel>> {
    Ok(Arc::new(windowed_model(
        25,
        25,
        100,
        2,
        b"artifact-account",
        entry_base,
    )?))
}

#[tokio::test]
async fn a_root_turn_past_the_window_reresolves_once_and_continues_on_the_fresh_model() -> TestResult
{
    let durable = Durable::new()?;
    let current = Arc::new(
        windowed_model(1, 9, WINDOW_END, 1, b"artifact-account", 9_100)?.with_tool_call_on(0),
    );
    let fresh = fresh_model(9_200)?;
    let registry = one_tool_registry()?;
    let reresolver = ScriptedReresolver::new([Ok(reresolved(
        &fresh,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        100,
    )?)]);
    let release = hold_first_call(&current);
    let agent = durable
        .sessions
        .create(
            "create:reresolve-root",
            durable.builder(&current, Some(Arc::clone(&reresolver)))?,
        )
        .await?;
    let created_binding = agent.checkpoint().manifest().binding().clone();
    let created_configuration = agent
        .checkpoint()
        .manifest()
        .provider_configuration()
        .clone();

    let turn = agent.prompt("use the probe while children run").await?;
    current.held_call_started.notified().await;
    // The window closes while the root's tool round is out.
    durable.clock.set(PAST_WINDOW);
    release.notify_one();
    let result = turn.result().await?;

    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(result.tool_calls(), 1);
    assert_eq!(reresolver.calls(), 1, "one re-resolution");
    assert_eq!(
        reresolver.accepted_windows(),
        [100],
        "the swap is reported accepted"
    );
    assert_eq!(current.calls.load(Ordering::Acquire), 1);
    assert_eq!(
        fresh.calls.load(Ordering::Acquire),
        1,
        "one dispatch after it"
    );
    assert_eq!(reresolutions(&durable.sink), [true]);
    // Canonical history and the durable binding are untouched by the swap.
    let manifest = agent.checkpoint().manifest().clone();
    assert_eq!(manifest.binding(), &created_binding);
    assert_eq!(manifest.provider_configuration(), &created_configuration);
    agent.shutdown().await?;
    let reopened = durable
        .reopen_exactly("open:reresolve-root", &agent, &fresh)
        .await?;
    assert_eq!(reopened, created_binding);
    Ok(())
}

#[tokio::test]
async fn a_new_turn_past_the_window_reresolves_before_its_first_dispatch() -> TestResult {
    let durable = Durable::new()?;
    let current = Arc::new(windowed_model(
        1,
        9,
        WINDOW_END,
        1,
        b"artifact-account",
        9_300,
    )?);
    let fresh = fresh_model(9_400)?;
    let registry = one_tool_registry()?;
    let reresolver = ScriptedReresolver::new([Ok(reresolved(
        &fresh,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        100,
    )?)]);
    let agent = durable
        .sessions
        .create(
            "create:reresolve-new-attempt",
            durable.builder(&current, Some(Arc::clone(&reresolver)))?,
        )
        .await?;
    let created_binding = agent.checkpoint().manifest().binding().clone();
    let first = agent.prompt("first assignment").await?.result().await?;
    assert!(matches!(first.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(
        reresolver.calls(),
        0,
        "inside the window nothing re-resolves"
    );
    let history_before = agent.checkpoint().manifest().canonical().history().clone();

    durable.clock.set(PAST_WINDOW);
    let second = agent.prompt("follow-up assignment").await?.result().await?;
    assert!(
        matches!(second.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        second.outcome()
    );
    assert_eq!(reresolver.calls(), 1);
    assert_eq!(current.calls.load(Ordering::Acquire), 1);
    assert_eq!(fresh.calls.load(Ordering::Acquire), 1);
    let history_after = agent.checkpoint().manifest().canonical().history().clone();
    assert!(
        history_after
            .segments()
            .iter()
            .take(history_before.segments().len())
            .eq(history_before.segments().iter()),
        "the swap rewrote no committed history"
    );
    agent.shutdown().await?;
    let reopened = durable
        .reopen_exactly("open:reresolve-new-attempt", &agent, &fresh)
        .await?;
    assert_eq!(reopened, created_binding);
    Ok(())
}

#[tokio::test]
async fn a_changed_account_or_a_refused_resolution_fails_closed_before_any_dispatch() -> TestResult
{
    for (case, other_account) in [("other account", true), ("refused resolution", false)] {
        let durable = Durable::new()?;
        let current = Arc::new(windowed_model(
            1,
            9,
            WINDOW_END,
            1,
            b"artifact-account",
            9_500,
        )?);
        let foreign = Arc::new(windowed_model(25, 25, 100, 2, b"other-account", 9_600)?);
        let registry = one_tool_registry()?;
        let answer = if other_account {
            Ok(reresolved(
                &foreign,
                &registry,
                LocalDurabilityDisposition::DurableRequired,
                100,
            )?)
        } else {
            Err(ModelReresolutionError::new(
                "fresh resolution changed the selected login; explicit rebind required",
            ))
        };
        let reresolver = ScriptedReresolver::new([answer]);
        let agent = durable
            .sessions
            .create(
                "create:reresolve-refused",
                durable.builder(&current, Some(Arc::clone(&reresolver)))?,
            )
            .await?;
        durable.clock.set(PAST_WINDOW);
        let result = agent.prompt("continue").await?.result().await?;
        let TurnResultOutcome::Failed(error) = result.outcome() else {
            return Err(format!("{case}: the turn did not fail: {:?}", result.outcome()).into());
        };
        assert_eq!(
            error.kind(),
            AgentErrorKind::ExternalBindingUnavailable,
            "{case}"
        );
        assert!(
            error.message().contains(if other_account {
                "fresh resolution changed"
            } else {
                "explicit rebind required"
            }),
            "{case}: {}",
            error.message()
        );
        assert_eq!(reresolver.calls(), 1, "{case}");
        assert_eq!(current.calls.load(Ordering::Acquire), 0, "{case}");
        assert_eq!(foreign.prepares.load(Ordering::Acquire), 0, "{case}");
        assert_eq!(foreign.calls.load(Ordering::Acquire), 0, "{case}");
        assert_eq!(reresolutions(&durable.sink), [false], "{case}");
        assert!(
            reresolver.accepted_windows().is_empty(),
            "{case}: a refusal is never reported accepted"
        );
        agent.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn without_a_reresolver_a_model_past_its_window_fails_as_before() -> TestResult {
    let durable = Durable::new()?;
    let current = Arc::new(windowed_model(
        1,
        9,
        WINDOW_END,
        1,
        b"artifact-account",
        9_700,
    )?);
    let agent = durable
        .sessions
        .create("create:no-reresolver", durable.builder(&current, None)?)
        .await?;
    durable.clock.set(PAST_WINDOW);
    let result = agent.prompt("continue").await?.result().await?;
    let TurnResultOutcome::Failed(error) = result.outcome() else {
        return Err(format!("the turn did not fail: {:?}", result.outcome()).into());
    };
    assert_ne!(error.kind(), AgentErrorKind::ExternalBindingUnavailable);
    assert_eq!(current.calls.load(Ordering::Acquire), 0);
    assert!(reresolutions(&durable.sink).is_empty());
    agent.shutdown().await?;
    Ok(())
}

/// Inside the window an installed reresolver changes nothing: the same
/// scripted turn commits the same history, revalidations and dispatches with
/// and without it.
#[tokio::test]
async fn inside_the_window_a_reresolver_changes_nothing() -> TestResult {
    let mut histories = Vec::new();
    for hooked in [false, true] {
        let durable = Durable::new()?;
        let current = Arc::new(
            windowed_model(1, 9, WINDOW_END, 1, b"artifact-account", 9_800)?.with_tool_call_on(0),
        );
        let reresolver = ScriptedReresolver::new([]);
        let agent = durable
            .sessions
            .create(
                "create:inside-window",
                durable.builder(&current, hooked.then(|| Arc::clone(&reresolver)))?,
            )
            .await?;
        let result = agent.prompt("use the probe").await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
        assert_eq!(reresolver.calls(), 0);
        histories.push((
            // Entry ids come from a random source; the rest is the history.
            agent
                .checkpoint()
                .manifest()
                .canonical()
                .history()
                .segments()
                .iter()
                .map(|segment| {
                    (
                        segment.kind(),
                        segment
                            .items()
                            .iter()
                            .map(|item| (item.actor(), item.provenance(), item.blocks().to_vec()))
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>(),
            current.authority_checks.load(Ordering::Acquire),
            current.calls.load(Ordering::Acquire),
        ));
        agent.shutdown().await?;
    }
    assert_eq!(histories[0], histories[1]);
    Ok(())
}

/// The window closes between preparation and the dispatch fence: the
/// prepared call is refused, not re-targeted, and nothing re-resolves.
#[tokio::test]
async fn a_prepared_call_is_never_re_targeted() -> TestResult {
    let durable = Durable::new()?;
    let stale_at_dispatch = {
        let model = windowed_model(1, 9, WINDOW_END, 1, b"artifact-account", 9_900)?;
        let expired = RuntimeModelAuthority::new(
            &model.descriptor,
            model.authority.account_binding(),
            model.authority.credential_authority_binding(),
            1,
            Timestamp::new(1, 0)?,
            Some(Timestamp::new(5, 0)?),
        )?;
        Arc::new(model.with_dispatch_authority(expired))
    };
    let fresh = fresh_model(10_000)?;
    let registry = one_tool_registry()?;
    let reresolver = ScriptedReresolver::new([Ok(reresolved(
        &fresh,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        100,
    )?)]);
    let agent = durable
        .sessions
        .create(
            "create:prepared-not-retargeted",
            durable.builder(&stale_at_dispatch, Some(Arc::clone(&reresolver)))?,
        )
        .await?;
    let result = agent.prompt("continue").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Failed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(reresolver.calls(), 0);
    assert_eq!(stale_at_dispatch.calls.load(Ordering::Acquire), 0);
    assert_eq!(fresh.prepares.load(Ordering::Acquire), 0);
    agent.shutdown().await?;
    Ok(())
}

/// After a swap, the provider-overflow compaction and the resend both run on
/// the fresh model.
#[tokio::test]
async fn compaction_after_a_swap_uses_the_fresh_model() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let clock = SettableClock::at(10);
    let registry = one_tool_registry()?;
    let current = Arc::new(windowed_model(
        1,
        9,
        WINDOW_END,
        1,
        b"artifact-account",
        10_100,
    )?);
    let fresh = Arc::new(
        windowed_model(25, 25, 100, 2, b"artifact-account", 10_200)?.with_provider_overflows([0]),
    );
    let reresolver = ScriptedReresolver::new([Ok(reresolved(
        &fresh,
        &registry,
        LocalDurabilityDisposition::Ephemeral,
        100,
    )?)]);
    let agent = durable_builder(
        &workspace,
        configured(&current, &registry, LocalDurabilityDisposition::Ephemeral)?,
        registry,
        Arc::clone(&clock),
        None,
    )?
    .with_scheduled_compaction_enabled(true)
    .model_reresolver(reresolver.clone(), Timestamp::new(WINDOW_END, 0)?)
    .build()?;
    for prompt in ["older replaceable intent", "second older intent"] {
        let result = agent.prompt(prompt).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    clock.set(PAST_WINDOW);
    let result = agent
        .prompt("answer after the window and the overflow")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(reresolver.calls(), 1);
    assert_eq!(current.paid_compaction_calls.load(Ordering::Acquire), 0);
    assert_eq!(fresh.paid_compaction_calls.load(Ordering::Acquire), 1);
    assert_eq!(
        fresh.calls.load(Ordering::Acquire),
        3,
        "overflow, fold, resend"
    );
    agent.shutdown().await?;
    Ok(())
}

/// A builder whose mutating tool call suspends at an approval barrier.
fn decision_builder(
    workspace: &Arc<WorkspacePolicy>,
    model: &Arc<ArtifactModel>,
    registry: &FrozenToolRegistry,
    clock: &Arc<SettableClock>,
) -> TestResult<AgentBuilder> {
    Ok(durable_builder(
        workspace,
        configured(model, registry, LocalDurabilityDisposition::DurableRequired)?,
        registry.clone(),
        Arc::clone(clock),
        None,
    )?
    .tool_policy(Arc::new(RestoreDecisionPolicy::new()?)))
}

async fn wait_for_decision(agent: &Agent) -> DurableSessionAddress {
    loop {
        let checkpoint = agent.checkpoint();
        if matches!(
            checkpoint.manifest().lifecycle(),
            RuntimeLifecycle::Working {
                recovery_phase: WorkingRecoveryPhase::EffectDecision,
                ..
            }
        ) {
            break DurableSessionAddress::new(
                agent.session_id(),
                agent.branch_id(),
                checkpoint.id(),
            );
        }
        tokio::task::yield_now().await;
    }
}

fn open_label(opened: &DurableRuntimeOpen) -> &'static str {
    match opened {
        DurableRuntimeOpen::Opened(_) => "Opened",
        DurableRuntimeOpen::RestoredWorking { .. } => "RestoredWorking",
        DurableRuntimeOpen::RecoveryRequired { .. } => "RecoveryRequired",
    }
}

/// Reopens an approval barrier, allows the pending effect and requires the
/// restored turn to complete.
async fn restore_and_allow(
    sessions: &DurableSessionRuntime,
    label: &str,
    address: DurableSessionAddress,
    builder: AgentBuilder,
) -> TestResult {
    let opened = sessions
        .open(label, address, builder)
        .await
        .map_err(|error| format!("open failed: {:?} {}", error.kind(), error.message()))?;
    let reopened_as = open_label(&opened);
    let DurableRuntimeOpen::RestoredWorking { agent, turn } = opened else {
        return Err(format!("the approval barrier reopened as {reopened_as}").into());
    };
    let decision = turn
        .pending_decision()
        .ok_or("restored effect decision was absent")?
        .clone();
    turn.control()
        .allow_effect(decision, EffectDecisionAuditId::new("test:restored-allow")?)
        .await?;
    let result = turn.result().result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    agent.shutdown().await?;
    Ok(())
}

struct SwappedDecision {
    _directory: tempfile::TempDir,
    workspace: Arc<WorkspacePolicy>,
    registry: FrozenToolRegistry,
    clock: Arc<SettableClock>,
    store: Arc<TestCreateStore>,
    sessions: DurableSessionRuntime,
    fresh: Arc<ArtifactModel>,
    original: Agent,
    address: DurableSessionAddress,
}

/// A turn that swapped in a fresh resolution past the window and then
/// suspended at an approval barrier: its frame's plan is bound to the fresh
/// descriptor while the checkpoint keeps the creation-time configuration.
async fn swapped_decision(entry_base: u128) -> TestResult<SwappedDecision> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let registry = one_mutating_tool_registry()?;
    let clock = SettableClock::at(10);
    let current = Arc::new(windowed_model(
        1,
        9,
        WINDOW_END,
        1,
        b"artifact-account",
        entry_base,
    )?);
    let fresh = Arc::new(
        windowed_model(25, 25, 100, 2, b"artifact-account", entry_base + 500)?.with_tool_first(),
    );
    let reresolver = ScriptedReresolver::new([Ok(reresolved(
        &fresh,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        100,
    )?)]);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let original = sessions
        .create(
            "create:swap-decision",
            decision_builder(&workspace, &current, &registry, &clock)?
                .model_reresolver(reresolver.clone(), Timestamp::new(WINDOW_END, 0)?),
        )
        .await?;
    clock.set(PAST_WINDOW);
    let _turn = original.prompt("mutate after the window").await?;
    let address = wait_for_decision(&original).await;
    assert_eq!(reresolver.calls(), 1);
    assert_eq!(current.calls.load(Ordering::Acquire), 0);
    assert_eq!(fresh.calls.load(Ordering::Acquire), 1);
    Ok(SwappedDecision {
        _directory: directory,
        workspace,
        registry,
        clock,
        store,
        sessions,
        fresh,
        original,
        address,
    })
}

/// Spec 01 §7.4: the session reopens exactly after a swap, also with a turn
/// suspended at an approval barrier, and the restored turn completes.
#[tokio::test]
async fn an_approval_suspended_turn_after_a_swap_reopens_and_completes() -> TestResult {
    let swapped = swapped_decision(84_000).await?;
    swapped.store.expire_active_lease();
    restore_and_allow(
        &swapped.sessions,
        "open:swap-decision",
        swapped.address,
        decision_builder(
            &swapped.workspace,
            &swapped.fresh,
            &swapped.registry,
            &swapped.clock,
        )?,
    )
    .await?;
    let _ = swapped.original.shutdown().await;
    Ok(())
}

/// Without any re-resolution: a session reopened past its window on a fresh
/// observation of the same model suspends at an approval barrier, and that
/// barrier reopens and completes on the same fresh observation.
#[tokio::test]
async fn an_approval_suspended_turn_after_a_fresh_reopen_restores_and_completes() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let registry = one_mutating_tool_registry()?;
    let clock = SettableClock::at(10);
    let current = Arc::new(windowed_model(
        1,
        9,
        WINDOW_END,
        1,
        b"artifact-account",
        85_000,
    )?);
    let fresh =
        Arc::new(windowed_model(25, 25, 100, 2, b"artifact-account", 85_500)?.with_tool_first());
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let created = sessions
        .create(
            "create:fresh-reopen-decision",
            decision_builder(&workspace, &current, &registry, &clock)?,
        )
        .await?;
    let address = DurableSessionAddress::new(
        created.session_id(),
        created.branch_id(),
        created.checkpoint().id(),
    );
    created.shutdown().await?;
    clock.set(PAST_WINDOW);
    let opened = sessions
        .open(
            "open:fresh-reopen-decision-1",
            address,
            decision_builder(&workspace, &fresh, &registry, &clock)?,
        )
        .await?;
    let reopened_as = open_label(&opened);
    let DurableRuntimeOpen::Opened(reopened) = opened else {
        return Err(format!("the first reopen was {reopened_as}").into());
    };
    let _turn = reopened.prompt("mutate after a fresh reopen").await?;
    let address = wait_for_decision(&reopened).await;
    store.expire_active_lease();
    restore_and_allow(
        &sessions,
        "open:fresh-reopen-decision-2",
        address,
        decision_builder(&workspace, &fresh, &registry, &clock)?,
    )
    .await?;
    let _ = reopened.shutdown().await;
    Ok(())
}

/// A provider-hosted continuation (server pause) persisted after a swap
/// reopens and completes without replaying the paused call.
#[tokio::test]
async fn a_server_pause_after_a_swap_reopens_and_completes() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let registry = one_tool_registry()?;
    let clock = SettableClock::at(10);
    let current = Arc::new(windowed_model(
        1,
        9,
        WINDOW_END,
        1,
        b"artifact-account",
        86_000,
    )?);
    // A server pause carries the family artifact its continuation resumes.
    let mut fresh = windowed_model(25, 25, 100, 2, b"artifact-account", 86_500)?;
    fresh.candidate_calls = 1;
    let fresh = Arc::new(fresh.with_provider_pauses([0]));
    let reresolver = ScriptedReresolver::new([Ok(reresolved(
        &fresh,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        100,
    )?)]);
    let store = Arc::new(TestCreateStore::default());
    store.block_after_promoted_hosted();
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let builder = |model: &Arc<ArtifactModel>| -> TestResult<AgentBuilder> {
        durable_builder(
            &workspace,
            configured(
                model,
                &registry,
                LocalDurabilityDisposition::DurableRequired,
            )?,
            registry.clone(),
            Arc::clone(&clock),
            None,
        )
    };
    let original = sessions
        .create(
            "create:swap-server-pause",
            builder(&current)?.model_reresolver(reresolver.clone(), Timestamp::new(WINDOW_END, 0)?),
        )
        .await?;
    clock.set(PAST_WINDOW);
    let _turn = original.prompt("pause after the window").await?;
    let address = loop {
        let checkpoint = original.checkpoint();
        if store.commits_blocked.load(Ordering::Acquire)
            && matches!(
                checkpoint.manifest().lifecycle(),
                RuntimeLifecycle::Working {
                    recovery_phase: WorkingRecoveryPhase::HostedContinuation,
                    ..
                }
            )
        {
            break DurableSessionAddress::new(
                original.session_id(),
                original.branch_id(),
                checkpoint.id(),
            );
        }
        tokio::task::yield_now().await;
    };
    assert_eq!(reresolver.calls(), 1);
    assert_eq!(fresh.calls.load(Ordering::Acquire), 1);
    original.abort_driver_for_test();
    tokio::task::yield_now().await;
    store.resume_commits();
    store.expire_active_lease();
    let opened = sessions
        .open("open:swap-server-pause", address, builder(&fresh)?)
        .await?;
    let reopened_as = open_label(&opened);
    let DurableRuntimeOpen::RestoredWorking { agent, turn } = opened else {
        return Err(format!("the server pause after a swap reopened as {reopened_as}").into());
    };
    let result = turn.result().result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(fresh.calls.load(Ordering::Acquire), 2);
    assert_eq!(current.calls.load(Ordering::Acquire), 0);
    agent.shutdown().await?;
    Ok(())
}

/// The frame of a swapped turn is restored against its provider operation's
/// prepared call, never against freshness timestamps, and every integrity
/// axis that does not age is still enforced: another account, another route,
/// another plan digest or another operation is refused, and the restored plan
/// stays bound to the observation that produced it.
#[tokio::test]
async fn a_restored_frame_refuses_a_foreign_model_account_plan_or_operation() -> TestResult {
    let swapped = swapped_decision(87_000).await?;
    let checkpoint = swapped.original.checkpoint();
    let manifest = checkpoint.manifest();
    let frame = decode_await_effect_decision_continuation(&checkpoint)
        .map_err(|error| format!("{:?} {}", error.kind(), error.message()))?;
    let restore =
        |configuration: &CompiledProviderConfiguration, operation: OperationId, digest: Digest| {
            crate::runtime::continuation::restore_frame_plan(
                configuration,
                manifest,
                operation,
                &frame.provider_request,
                digest,
            )
        };

    let plan = restore(
        manifest.provider_configuration(),
        frame.provider_operation,
        frame.provider_plan_digest,
    )
    .map_err(|error| error.message().to_owned())?;
    assert_eq!(plan.digest(), frame.provider_plan_digest);
    // Bound to the fresh observation that produced it, not the checkpoint's.
    assert!(
        swapped
            .fresh
            .descriptor
            .validate_plan_binding(&plan)
            .is_ok()
    );
    assert!(
        manifest
            .provider_configuration()
            .effective()
            .descriptor()
            .validate_plan_binding(&plan)
            .is_err()
    );

    let foreign_account = Arc::new(windowed_model(25, 25, 100, 2, b"other-account", 87_900)?);
    let foreign_route = {
        let descriptor = durable_test_descriptor_at("other.route", Timestamp::new(25, 0)?)?;
        let authority = artifact_authority(&descriptor, 2)?;
        Arc::new(ArtifactModel::new(descriptor, authority, 0, 87_950, false))
    };
    for (label, foreign) in [("account", foreign_account), ("route", foreign_route)] {
        let configured = configured(
            &foreign,
            &swapped.registry,
            LocalDurabilityDisposition::DurableRequired,
        )?;
        assert!(
            restore(
                configured.configuration(),
                frame.provider_operation,
                frame.provider_plan_digest,
            )
            .is_err(),
            "a configuration with another {label} restored the frame"
        );
    }
    assert!(
        restore(
            manifest.provider_configuration(),
            frame.provider_operation,
            Digest::sha256(DigestDomain::RequestPlan, 3, &[b"another plan"])?,
        )
        .is_err(),
        "another plan digest restored the frame"
    );
    assert!(
        restore(
            manifest.provider_configuration(),
            frame.decision_operation,
            frame.provider_plan_digest,
        )
        .is_err(),
        "a non-provider operation restored the frame"
    );
    let _ = swapped.original.shutdown().await;
    Ok(())
}
