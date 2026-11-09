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
}

impl RunnerLink for InProcessRunner {
    fn call(&self, call: RunnerCall) -> RunnerCallFuture {
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
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000071".parse()?),
        root,
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let sessions = line_sessions(&environment, &job_capacity(8))?;
    let descriptor = runner_descriptor(&environment, plakous_tools::RunnerId::new("remote-held")?)?;
    let backend = RunnerBackend::new(
        descriptor,
        Arc::new(InProcessRunner {
            environment,
            sessions,
        }),
    )?;
    Ok(RunnerServices::new(
        Arc::new(backend),
        tokio::runtime::Handle::current(),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hold_after_preparation_is_excluded_from_the_model_budget() -> TestResult {
    let model = artifact_test_model(0, 141_000)?;
    let (agent, services, _directory) = held_agent(
        Arc::clone(&model),
        Some(Duration::from_millis(400)),
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
    tokio::time::sleep(Duration::from_millis(800)).await;
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
