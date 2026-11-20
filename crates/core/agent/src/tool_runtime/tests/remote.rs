use super::*;
use crate::tool_runtime::{
    DurableToolDispatchObserver, RemoteRevalidationFuture, ToolDispatchObserver,
    ToolDispatchObserverFuture, ToolDispatchPermit, ToolDispatchState, ToolExecutionControl,
    ToolRuntimeConfig, ToolScheduler, ToolTimer, ToolTimerFuture,
};
use plakous_model::{CancellationToken, Clock, SystemClock};
use plakous_tools::internal::runner::{
    RunnerBackend, RunnerCall, RunnerCallFuture, RunnerFailure, RunnerLink, RunnerVerb,
    runner_descriptor, runner_environment, serve_probe,
};
use plakous_tools::{AlwaysAllowPolicy, ProgressSink, RunnerId, ToolRuntimeErrorKind};
use tokio::sync::{Mutex, Notify};

#[derive(Default)]
struct ProbeLink {
    state: AtomicUsize,
    entered: Arc<Notify>,
}

impl RunnerLink for ProbeLink {
    fn call(&self, call: RunnerCall) -> RunnerCallFuture {
        assert_eq!(call.verb(), RunnerVerb::ProbeWorkspace);
        if call.payload()["op"] == "filesystem_identity" {
            match self.state.load(Ordering::Acquire) {
                1 => {
                    return Box::pin(async {
                        Err(RunnerFailure::NotSent("fixture outage".into()))
                    });
                }
                2 => {
                    let entered = Arc::clone(&self.entered);
                    return Box::pin(async move {
                        entered.notify_one();
                        std::future::pending().await
                    });
                }
                3 => return Box::pin(async { Ok(json!({"device":42,"inode":24})) }),
                _ => {}
            }
        }
        let answer = serve_probe(call.payload().clone(), &|_| true);
        Box::pin(async move { answer.await.map_err(|_| RunnerFailure::Unsupported) })
    }
}

#[derive(Default)]
struct Observer(Mutex<()>);

impl ToolDispatchObserver for Observer {
    fn dispatch_started(&self) -> ToolDispatchObserverFuture<'_> {
        panic!("remote durable dispatch must not use the synchronous observer");
    }
}

impl DurableToolDispatchObserver for Observer {
    fn dispatch_started_remote<'a>(
        &'a self,
        revalidate: &'a (dyn Fn() -> RemoteRevalidationFuture<'a> + Sync),
    ) -> ToolDispatchObserverFuture<'a> {
        Box::pin(async move {
            drop(self.0.lock().await);
            revalidate().await?;
            let guard = self.0.lock().await;
            Ok(ToolDispatchPermit::new(move || {
                drop(guard);
                Ok(())
            }))
        })
    }
}

struct TriggerTimer(CancellationToken);
impl ToolTimer for TriggerTimer {
    fn sleep_until(&self, _: std::time::Instant) -> ToolTimerFuture {
        let trigger = self.0.clone();
        Box::pin(async move { trigger.cancelled().await })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_claims_check_live_only_at_dispatch_and_interrupt_a_blocked_probe()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let run = RunId::try_from(uuid::Uuid::from_u128(661))?;
    let config = ExecutionEnvironmentConfig::default();
    let scope = ExecutionEnvironmentScope::Run(run);
    let host = ExecutionEnvironment::host(scope, directory.path(), ".", config.clone())?;
    let link = Arc::new(ProbeLink::default());
    let backend = Arc::new(RunnerBackend::new(
        runner_descriptor(&host, RunnerId::new("fixture")?)?,
        link.clone(),
    )?);
    let environment = Arc::new(runner_environment(
        scope,
        directory.path(),
        ".",
        config,
        backend,
    )?);
    let name = ToolName::new("fixed")?;
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("fixture.fixed")?,
        ToolExecutorId::new("fixture.fixed")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"fixture.fixed"])?,
        ToolDefinition::new(
            name.clone(),
            "1.0.0",
            "Return a fixed result.",
            ToolInputContract::utf8(100)?,
            ToolOutputContract::portable_model_content(),
            ToolConcurrency::Serial,
            ToolEffect::Mutating,
        )?,
        Arc::new(FixedTool(ToolOutcome::success_text("started")?)),
    ))?;
    let limit = CancellationToken::new();
    let runtime = ToolRuntime::with_scheduler_and_timer(
        registry.freeze(),
        environment,
        Arc::new(AlwaysAllowPolicy),
        ToolScheduler::new(1)?,
        ToolRuntimeConfig::new(1)?,
        Arc::new(TriggerTimer(limit.clone())),
    );
    let turn = TurnId::try_from(uuid::Uuid::from_u128(662))?;
    let attempt = AttemptId::try_from(uuid::Uuid::from_u128(663))?;
    for (index, (mode, expected)) in [
        (1, Some(ToolRuntimeErrorKind::Execution)),
        (3, Some(ToolRuntimeErrorKind::Policy)),
        (0, None),
        (2, Some(ToolRuntimeErrorKind::Cancelled)),
        (2, Some(ToolRuntimeErrorKind::ExecutionTimeout)),
    ]
    .into_iter()
    .enumerate()
    {
        let call = CallId::try_from(uuid::Uuid::from_u128(670 + index as u128))?;
        let invocation = runtime.prepare(ToolInvocationRequest::new(
            run,
            turn,
            attempt,
            call,
            name.clone(),
            ToolCorrelation::default(),
            "input",
        ))?;
        let binding = runtime.binding_for(&invocation)?;
        // Neither restoring a pending decision nor resolving or journaling its
        // exact authority should ask the unavailable runner.
        link.state.store(1, Ordering::Release);
        let pending = runtime.restore_pending_effect_decision(
            invocation,
            binding.digest(),
            EffectDecisionPrompt::new("restore"),
        )?;
        let EffectAuthorization::Allowed(authorized) = runtime.resolve_effect_decision(
            pending,
            EffectDecision::allow(
                binding.digest(),
                EffectDecisionAuditId::new("fixture:allow")?,
            ),
        )?
        else {
            return Err("expected exact allow".into());
        };
        let intent = runtime.effect_dispatch_intent(&authorized)?;
        let claim = Digest::sha256(DigestDomain::Snapshot, 1, &[b"fixture-claim"])?;
        let claimed = runtime.claim_effect(authorized, intent, claim)?;

        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let (progress, _receiver) = ProgressSink::channel(2, call, Arc::clone(&clock))?;
        let cancel = CancellationToken::new();
        let observer = Arc::new(Observer::default());
        let control = ToolExecutionControl::new(cancel.clone(), None, clock, progress)
            .with_durable_dispatch_observer(observer.clone());
        link.state.store(mode, Ordering::Release);
        let execution = runtime.execute_claimed(claimed, control);
        tokio::pin!(execution);
        if mode == 2 {
            tokio::select! {
                () = link.entered.notified() => {}
                _ = &mut execution => return Err("dispatch did not wait for the remote probe".into()),
            }
            assert!(
                observer.0.try_lock().is_ok(),
                "probe held the dispatch gate"
            );
            if expected == Some(ToolRuntimeErrorKind::Cancelled) {
                cancel.cancel();
            } else {
                limit.cancel();
            }
        }
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), execution).await?;
        if let Some(expected) = expected {
            assert_eq!(result.state(), ToolDispatchState::NotStarted);
            let error = result.error().ok_or("expected refusal")?;
            assert_eq!(error.kind(), expected);
            if expected == ToolRuntimeErrorKind::Execution {
                assert!(error.proven_no_effect());
            }
        } else {
            assert_eq!(result.state(), ToolDispatchState::Started);
            assert!(result.outcome().is_some());
        }
    }
    Ok(())
}
