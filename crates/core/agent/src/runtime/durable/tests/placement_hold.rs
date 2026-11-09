//! A held runner placement parks new paid model work before dispatch: no
//! provider call, no dispatch gate, no request slot and no model budget
//! while held, and automatic release after its owner reopens admission.

use super::*;
use plakous_tools::internal::runner::{
    RunnerBackend, RunnerCall, RunnerCallFuture, RunnerFailure, RunnerLink, RunnerServices,
    RunnerVerbFamily, ServeFailure, job_capacity, line_sessions, runner_descriptor, serve,
    serve_probe, serve_sessions,
};
use std::time::Duration;

/// Serves probes, environment and session calls in process over the test
/// workspace, as the runner side would.
struct InProcessRunner {
    environment: ExecutionEnvironment,
    sessions: plakous_tools::JoinedProcessSessions,
    /// Process executions the runner was sent.
    effects: AtomicUsize,
}

impl RunnerLink for InProcessRunner {
    fn call(&self, call: RunnerCall) -> RunnerCallFuture {
        if call.verb() == plakous_tools::internal::runner::RunnerVerb::ExecuteLiteralProcess {
            self.effects.fetch_add(1, Ordering::SeqCst);
        }
        let control = plakous_tools::OperationContext::new(call.cancellation().clone());
        let served = match call.verb().family() {
            RunnerVerbFamily::Probe => serve_probe(call.payload().clone(), &|_| true),
            RunnerVerbFamily::Environment => serve(
                &self.environment,
                call.verb().as_str(),
                call.payload().clone(),
                control,
            ),
            RunnerVerbFamily::Sessions => serve_sessions(
                &self.environment,
                &self.sessions,
                "instance",
                call.verb().as_str(),
                call.payload().clone(),
                control,
                false,
            ),
            _ => return Box::pin(std::future::ready(Err(RunnerFailure::Unsupported))),
        };
        Box::pin(async move {
            served.await.map_err(|failure| match failure {
                ServeFailure::Backend { error, .. } => RunnerFailure::Backend(error),
                ServeFailure::Unsupported => RunnerFailure::Unsupported,
            })
        })
    }
}

fn placed_services(root: &std::path::Path) -> TestResult<RunnerServices> {
    Ok(placed(root)?.0)
}

fn placed(root: &std::path::Path) -> TestResult<(RunnerServices, Arc<InProcessRunner>)> {
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000071".parse()?),
        root,
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let sessions = line_sessions(&environment, &job_capacity(8))?;
    let descriptor = runner_descriptor(&environment, plakous_tools::RunnerId::new("remote-held")?)?;
    let runner = Arc::new(InProcessRunner {
        environment,
        sessions,
        effects: AtomicUsize::new(0),
    });
    let link: Arc<dyn RunnerLink> = runner.clone();
    let backend = RunnerBackend::new(descriptor, link)?;
    Ok((
        RunnerServices::new(Arc::new(backend), tokio::runtime::Handle::current()),
        runner,
    ))
}

/// Holds the placement once the round is prepared, so the wait falls inside
/// the model operation's own budget window, after its start.
struct HoldAfterPrepare {
    inner: Arc<ArtifactModel>,
    services: RunnerServices,
    hold: AtomicBool,
}

impl LanguageModel for HoldAfterPrepare {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        Arc::clone(&self.inner).revalidate_runtime_authority(checked_at)
    }

    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        if self.hold.swap(false, Ordering::AcqRel) {
            self.services.set_available(false);
        }
        Arc::clone(&self.inner).prepare_call(plan, context)
    }
}

async fn held_agent(
    model: Arc<ArtifactModel>,
    timeout: Option<Duration>,
    hold_after_prepare: bool,
    label: &str,
) -> TestResult<(Agent, RunnerServices, tempfile::TempDir)> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let services = placed_services(directory.path())?;
    let mut builder = durable_artifact_test_builder(workspace, Arc::clone(&model))
        .with_runner_services(services.clone());
    if hold_after_prepare {
        builder.model = Arc::new(HoldAfterPrepare {
            inner: model,
            services: services.clone(),
            hold: AtomicBool::new(true),
        });
    }
    if let Some(timeout) = timeout {
        builder.config = builder.config.with_model_operation_timeout(timeout);
    }
    let runtime = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = runtime.create(&format!("create:{label}"), builder).await?;
    Ok((agent, services, directory))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_placement_parks_the_first_dispatch_until_release() -> TestResult {
    let model = artifact_test_model(0, 140_000)?;
    let (agent, services, _directory) =
        held_agent(Arc::clone(&model), None, false, "placement-hold-first").await?;
    services.set_available(false);
    let turn = agent
        .start(
            CommandId::new("placement-hold-first:start")?,
            "wait for the runner",
        )
        .await?
        .into_turn()
        .ok_or("turn")?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        0,
        "no paid dispatch while the placement is held"
    );
    // The wait keeps neither the per-session dispatch gate nor the turn.
    let gate = Arc::clone(&agent.inner.provider_dispatch_gate);
    drop(tokio::time::timeout(Duration::from_secs(30), gate.lock()).await?);
    services.set_available(true);
    let result = tokio::time::timeout(Duration::from_secs(30), turn.result()).await??;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    agent.shutdown().await?;
    Ok(())
}

/// The model operation budget of the hold-after-preparation case: long
/// enough that preparation and dispatch never spend it under load, and the
/// hold spends it twice over.
const BUDGET: Duration = Duration::from_secs(2);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hold_after_preparation_is_excluded_from_the_model_budget() -> TestResult {
    let model = artifact_test_model(0, 141_000)?;
    let (agent, services, _directory) = held_agent(
        Arc::clone(&model),
        Some(BUDGET),
        true,
        "placement-hold-budget",
    )
    .await?;
    let turn = agent
        .start(
            CommandId::new("placement-hold-budget:start")?,
            "the runner leaves after preparation",
        )
        .await?
        .into_turn()
        .ok_or("turn")?;
    tokio::time::timeout(Duration::from_secs(30), async {
        while model.prepares.load(Ordering::Acquire) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    // Twice the whole model budget passes while held.
    tokio::time::sleep(BUDGET * 2).await;
    assert_eq!(model.calls.load(Ordering::Acquire), 0);
    services.set_available(true);
    let result = tokio::time::timeout(Duration::from_secs(30), turn.result()).await??;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "the hold spent the model budget: {:?}",
        result.outcome()
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_ends_a_held_turn_without_dispatch_while_the_runner_stays_absent() -> TestResult {
    let model = artifact_test_model(0, 142_000)?;
    let (agent, services, _directory) =
        held_agent(Arc::clone(&model), None, false, "placement-hold-stop").await?;
    services.set_available(false);
    let turn = agent
        .start(
            CommandId::new("placement-hold-stop:start")?,
            "stop me while the runner is gone",
        )
        .await?
        .into_turn()
        .ok_or("turn")?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let control = turn.control();
    tokio::time::timeout(Duration::from_secs(10), control.cancel()).await??;
    let result = tokio::time::timeout(Duration::from_secs(10), turn.result()).await??;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Cancelled(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 0);
    assert!(!services.is_available(), "Stop never releases the hold");
    // Job cleanup at shutdown is a control: it would fail fast while held.
    services.set_available(true);
    agent.shutdown().await?;
    Ok(())
}

/// A mutating tool that loses its placement just before it runs one
/// process on the runner, so the call is held mid-batch.
struct HeldEffect {
    services: RunnerServices,
    started: AtomicBool,
}

impl plakous_tools::ToolExecutor for HeldEffect {
    fn execute(
        self: Arc<Self>,
        _invocation: ToolInvocation,
        context: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        Box::pin(async move {
            self.services.set_available(false);
            self.started.store(true, Ordering::Release);
            let request = plakous_tools::LiteralProcessRequest::new(
                "/bin/sh",
                ["-c".to_owned(), "touch marker".to_owned()],
                ".",
                Vec::new(),
                plakous_tools::LiteralProcessIntent::default(),
            )
            .map_err(plakous_tools::backend_runtime_error)?;
            let ran = context
                .environment()
                .execute_literal_process(request, plakous_tools::operation_context(&context))
                .await;
            Err(ran.map_or_else(plakous_tools::backend_runtime_error, |_| {
                ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, "the held effect ran")
            }))
        })
    }
}

fn held_effect_registry(tool: &Arc<HeldEffect>) -> TestResult<FrozenToolRegistry> {
    let held = Arc::clone(tool);
    effect_registry(held)
}

fn effect_registry(
    executor: Arc<dyn plakous_tools::ToolExecutor>,
) -> TestResult<FrozenToolRegistry> {
    let definition = ToolDefinition::new(
        ToolName::new("probe")?,
        "1.0.0",
        "Run one process on the runner.",
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
        executor,
    ))?;
    Ok(registry.freeze())
}

struct HeldBatch {
    agent: Agent,
    turn: crate::runtime::Turn,
    services: RunnerServices,
    runner: Arc<InProcessRunner>,
    directory: tempfile::TempDir,
}

/// A turn whose one mutating runner call is held mid-batch while the
/// runner stays absent.
async fn held_batch(label: &str, base: u128) -> TestResult<HeldBatch> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (services, runner) = placed(directory.path())?;
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            1,
            base,
            false,
        )
        .with_tool_first(),
    );
    let tool = Arc::new(HeldEffect {
        services: services.clone(),
        started: AtomicBool::new(false),
    });
    let builder = durable_decision_builder(workspace, model, held_effect_registry(&tool)?)?
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .with_scheduled_compaction_enabled(false)
        .with_runner_services(services.clone());
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions.create(&format!("create:{label}"), builder).await?;
    let turn = agent.prompt("run it on the runner").await?;
    tokio::time::timeout(Duration::from_secs(30), async {
        while !tool.started.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(runner.effects.load(Ordering::SeqCst), 0, "held, not sent");
    Ok(HeldBatch {
        agent,
        turn,
        services,
        runner,
        directory,
    })
}

/// Stop reaches a runner call held mid-batch through its owner while the
/// runner stays absent: the turn ends cancelled within a bound, nothing is
/// sent, and the hold stays until its owner releases it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_ends_a_turn_with_a_runner_call_held_mid_batch() -> TestResult {
    let held = held_batch("placement-hold-batch-stop", 143_000).await?;
    let control = held.turn.control();
    tokio::time::timeout(Duration::from_secs(10), control.cancel()).await??;
    let result = tokio::time::timeout(Duration::from_secs(10), held.turn.result()).await??;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Cancelled(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(held.runner.effects.load(Ordering::SeqCst), 0);
    assert!(!held.directory.path().join("marker").exists());
    assert!(
        !held.services.is_available(),
        "Stop never releases the hold"
    );
    held.services.set_available(true);
    held.agent.shutdown().await?;
    Ok(())
}

/// Shutdown reaches a held mutating runner call too, with the placement
/// still held: it completes within a bound and sends nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_ends_a_held_mutating_runner_call_without_sending() -> TestResult {
    let held = held_batch("placement-hold-batch-shutdown", 144_000).await?;
    let _ = tokio::time::timeout(Duration::from_secs(10), held.agent.shutdown()).await?;
    let _ = tokio::time::timeout(Duration::from_secs(10), held.turn.result()).await?;
    assert_eq!(held.runner.effects.load(Ordering::SeqCst), 0);
    assert!(!held.directory.path().join("marker").exists());
    assert!(!held.services.is_available());
    Ok(())
}

/// A driver parked on its placement holds no shared request slot: another
/// agent on the same single slot completes a turn meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parked_driver_leaves_the_sole_request_slot_to_another_agent() -> TestResult {
    let slots =
        crate::runtime::RequestSlots::new("one", std::num::NonZeroUsize::new(1).ok_or("one slot")?);
    let held_model = artifact_test_model(0, 145_000)?;
    let held_directory = tempfile::tempdir()?;
    let (services, _runner) = placed(held_directory.path())?;
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let held = sessions
        .create(
            "create:placement-hold-slot-held",
            durable_artifact_test_builder(
                Arc::new(WorkspacePolicy::host(held_directory.path(), ".")?),
                Arc::clone(&held_model),
            )
            .with_runner_services(services.clone())
            .request_slots(slots.clone()),
        )
        .await?;
    services.set_available(false);
    let parked = held.prompt("wait for the runner").await?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let other_directory = tempfile::tempdir()?;
    let other_model = artifact_test_model(0, 146_000)?;
    let other_sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let other = other_sessions
        .create(
            "create:placement-hold-slot-other",
            durable_artifact_test_builder(
                Arc::new(WorkspacePolicy::host(other_directory.path(), ".")?),
                Arc::clone(&other_model),
            )
            .request_slots(slots),
        )
        .await?;
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        other.prompt("the slot is free").await?.result(),
    )
    .await??;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(held_model.calls.load(Ordering::Acquire), 0);
    services.set_available(true);
    let parked = tokio::time::timeout(Duration::from_secs(30), parked.result()).await??;
    assert!(matches!(parked.outcome(), TurnResultOutcome::Completed(_)));
    other.shutdown().await?;
    held.shutdown().await?;
    Ok(())
}

/// An idle session-workflow compaction parked on a held placement has no
/// turn to cancel; its owner's cooperative interruption ends the wait,
/// with no paid dispatch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_idle_compaction_parked_on_its_placement_observes_interruption() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let services = placed_services(directory.path())?;
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:placement-hold-compaction",
            durable_artifact_test_builder(Arc::clone(&workspace), artifact_test_model(0, 147_000)?)
                .with_runner_services(services.clone()),
        )
        .await?;
    for prompt in ["older replaceable intent", "latest protected intent"] {
        let result = agent.prompt(prompt).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    let source = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    let (builder, model) = recovery_compaction_fixture(
        workspace,
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    let builder = builder.with_runner_services(services.clone());
    let interrupted = Arc::new(AtomicBool::new(false));
    let probe = Arc::clone(&interrupted);
    agent.set_storage_interrupt(Arc::new(move || probe.load(Ordering::Acquire)));
    services.set_available(false);
    let endpoint = agent.session_endpoint();
    let compact = tokio::spawn(async move {
        endpoint
            .compact_session(AgentCompactSessionRequest::new(
                "compact:placement-hold".to_owned(),
                source.session_id,
                source.branch_id,
                source.checkpoint_id,
                AgentCompactionReason::Manual,
                builder,
            ))
            .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!compact.is_finished(), "the compaction parks on the hold");
    assert_eq!(model.starts.load(Ordering::Acquire), 0);
    interrupted.store(true, Ordering::Release);
    let result = tokio::time::timeout(Duration::from_secs(10), compact).await??;
    assert!(
        !matches!(result, Ok(AgentCompactSessionOutcome::Compacted { .. })),
        "{result:?}"
    );
    assert_eq!(model.starts.load(Ordering::Acquire), 0);
    services.set_available(true);
    agent.shutdown().await?;
    Ok(())
}

/// A mutating tool that runs until released, noting whether its call was
/// cancelled meanwhile.
struct GatedEffect {
    release: Notify,
    started: AtomicBool,
    cancelled: AtomicBool,
}

impl plakous_tools::ToolExecutor for GatedEffect {
    fn execute(
        self: Arc<Self>,
        _invocation: ToolInvocation,
        context: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        Box::pin(async move {
            self.started.store(true, Ordering::Release);
            tokio::select! {
                () = self.release.notified() => {}
                () = context.cancellation().cancelled() => {
                    self.cancelled.store(true, Ordering::Release);
                }
            }
            Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "the gated effect ended",
            ))
        })
    }
}

/// A transfer shutdown on an open placement leaves a running mutating tool
/// to finish rather than cancelling it: only a hold ends it at shutdown.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_shutdown_lets_an_unheld_mutating_tool_finish() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let services = placed_services(directory.path())?;
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            1,
            148_000,
            false,
        )
        .with_tool_first(),
    );
    let tool = Arc::new(GatedEffect {
        release: Notify::new(),
        started: AtomicBool::new(false),
        cancelled: AtomicBool::new(false),
    });
    let gated = Arc::clone(&tool);
    let builder = durable_decision_builder(workspace, model, effect_registry(gated)?)?
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .with_scheduled_compaction_enabled(false)
        .with_runner_services(services.clone());
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create("create:placement-hold-transfer", builder)
        .await?;
    let _turn = agent.prompt("run until released").await?;
    tokio::time::timeout(Duration::from_secs(30), async {
        while !tool.started.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    let shutdown = {
        let agent = agent.clone();
        tokio::spawn(async move { crate::runtime::shutdown_for_transfer(&agent).await })
    };
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        !tool.cancelled.load(Ordering::Acquire),
        "shutdown waits for the tool"
    );
    assert!(!shutdown.is_finished());
    tool.release.notify_one();
    let _ = tokio::time::timeout(Duration::from_secs(30), shutdown).await??;
    assert!(!tool.cancelled.load(Ordering::Acquire));
    Ok(())
}
