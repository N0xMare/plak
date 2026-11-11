use super::super::code_cell_composition::{
    FakeCompositionEvidence, composition_projection_target, composition_test_builder,
    neutral_composition_model, neutral_composition_registry_with_tools,
    neutral_composition_test_descriptor,
};
use super::*;
use crate::local_composition::{LocalCompositionNestedDelivery, LocalCompositionNestedResponder};

struct Counted(AtomicUsize);

impl plakous_tools::ToolExecutor for Counted {
    fn execute(
        self: Arc<Self>,
        _: ToolInvocation,
        _: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            ToolOutcome::success_text("nested once")
        })
    }
}

struct Replied(tokio::sync::oneshot::Sender<Result<(), String>>);

impl LocalCompositionNestedResponder for Replied {
    fn resolve(
        self: Box<Self>,
        _: serde_json::Value,
    ) -> LocalCompositionFuture<'static, LocalCompositionNestedDelivery> {
        Box::pin(async move {
            let _ = self.0.send(Ok(()));
            Ok(LocalCompositionNestedDelivery::Accepted)
        })
    }
    fn reject(
        self: Box<Self>,
        reason: String,
    ) -> LocalCompositionFuture<'static, LocalCompositionNestedDelivery> {
        Box::pin(async move {
            let _ = self.0.send(Err(reason));
            Ok(LocalCompositionNestedDelivery::Accepted)
        })
    }
}

async fn cell(
    directory: &std::path::Path,
    services: &RunnerServices,
    counted: &Arc<Counted>,
) -> TestResult<(Agent, Arc<ArtifactModel>, Arc<FakeCompositionEvidence>)> {
    let registry = neutral_composition_registry_with_tools(
        false,
        vec![effect_registration(counted.clone())?.nested_eligible()],
    )?;
    let descriptor = neutral_composition_test_descriptor()?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(
        ArtifactModel::new(descriptor.clone(), authority.clone(), 0, 148_000, false)
            .with_scripted_tool_calls([(0, "exec", serde_json::to_string("seed job")?)]),
    );
    let configured =
        neutral_composition_model(&model, descriptor.clone(), authority.clone(), &registry)?;
    let target = composition_projection_target(&configured, &descriptor)?;
    let composition = ledger_review::review_composition(registry.clone(), target, false)?;
    let evidence = Arc::new(FakeCompositionEvidence::default());
    let mut builder = composition_test_builder(
        Arc::new(WorkspacePolicy::host(directory, ".")?),
        registry,
        &model,
        descriptor,
        authority,
        composition,
        (evidence.clone(), LocalCompositionLifecycle::RootTurnJoined),
    )?
    .with_runner_services(services.clone());
    builder.harness_plan = Some(compile_explicit_runtime_profile(
        builder
            .harness_plan
            .as_ref()
            .ok_or("plan")?
            .source_profile(),
        model.as_ref(),
        &builder.provider_configuration,
        &builder.tools,
        &builder.workspace,
        services.backend().descriptor(),
        Vec::new(),
    )?);
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions.create("create:placement-nested", builder).await?;
    Ok((agent, model, evidence))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn placement_hold_cut_queued_nested_work_waits_without_executor_admission_and_runs_once()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let services = placed_services(directory.path())?;
    let counted = Arc::new(Counted(AtomicUsize::new(0)));
    let (agent, model, evidence) = cell(directory.path(), &services, &counted).await?;
    let turn = agent.prompt("keep queued nested work").await?;
    tokio::time::timeout(Duration::from_secs(30), async {
        while evidence.physical_cell_starts.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    services.set_available(false);
    let (sent, mut response) = tokio::sync::oneshot::channel();
    evidence
        .events
        .lock()
        .map_err(|_| "events")?
        .as_ref()
        .ok_or("events")?
        .send(LocalCompositionEvent::NestedCall {
            call: crate::local_composition::LocalCompositionNestedCall {
                cell_id: "C".into(),
                runtime_call_id: "held-nested".into(),
                tool_name: "probe".into(),
                input: Some(json!({})),
            },
            responder: Box::new(Replied(sent)),
        })
        .map_err(|_| "event delivery")?;
    let held = tokio::time::timeout(Duration::from_millis(100), &mut response).await;
    assert!(
        held.is_err(),
        "held nested call was admitted and delivered: {held:?}"
    );
    assert_eq!(
        counted.0.load(Ordering::SeqCst),
        0,
        "held nested executor started"
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    services.set_available(true);
    tokio::time::timeout(Duration::from_secs(30), response)
        .await??
        .map_err(|error| format!("nested refusal: {error}"))?;
    assert_eq!(counted.0.load(Ordering::SeqCst), 1);
    evidence.source_release.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(30), turn.result()).await??;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(evidence.physical_cell_starts.load(Ordering::SeqCst), 1);
    agent.shutdown().await?;
    Ok(())
}
