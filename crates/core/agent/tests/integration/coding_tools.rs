//! Disposable-workspace conformance for the provider-neutral coding pack.

#![cfg(target_os = "linux")]

use std::error::Error;
use std::fs;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

use plakous_agent::internal::{EffectAuthorization, ToolExecutionControl, ToolRuntime};
use plakous_model::{
    AttemptId, CallId, CancellationToken, Clock, Digest, DigestDomain, FixedClock, RunId,
    Timestamp, ToolCorrelation, ToolName, ToolResultPart, TurnId,
};
use plakous_tool_plan::{InMemoryPlanState, PlanState};
use plakous_tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, ProgressSink,
    ReadOnlyToolPolicy, ToolExposure, ToolExposureMap, ToolInvocation, ToolInvocationRequest,
    ToolOutcome, ToolOutcomeKind, ToolPolicy, ToolPolicyDecision, ToolPolicyFuture,
    ToolPolicyRevision, ToolRegistryBuilder,
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

#[derive(Clone, Copy)]
struct TestIds {
    run: RunId,
    turn: TurnId,
    attempt: AttemptId,
    call: CallId,
}

fn ids() -> TestResult<TestIds> {
    Ok(TestIds {
        run: RunId::from_str("00000000-0000-0000-0000-000000000061")?,
        turn: TurnId::from_str("00000000-0000-0000-0000-000000000062")?,
        attempt: AttemptId::from_str("00000000-0000-0000-0000-000000000063")?,
        call: CallId::from_str("00000000-0000-0000-0000-000000000064")?,
    })
}

fn with_call(ids: TestIds, suffix: u8) -> TestResult<TestIds> {
    Ok(TestIds {
        call: CallId::from_str(&format!("00000000-0000-0000-0000-{suffix:012x}"))?,
        ..ids
    })
}

fn registry() -> TestResult<plakous_tools::FrozenToolRegistry> {
    registry_with_plan(Arc::new(InMemoryPlanState::new()))
}

fn registry_with_plan(plan: Arc<dyn PlanState>) -> TestResult<plakous_tools::FrozenToolRegistry> {
    let packs: Vec<Arc<dyn plakous_tools::ToolPack>> = vec![
        Arc::new(plakous_tool_files::FilesToolPack),
        Arc::new(plakous_tool_search::SearchToolPack),
        Arc::new(plakous_tool_plan::PlanToolPack::new(plan)),
    ];
    let mut builder = ToolRegistryBuilder::default();
    plakous_tools::register_packs(&packs, &mut builder)?;
    Ok(builder.freeze())
}

fn runtime(
    registry: plakous_tools::FrozenToolRegistry,
    workspace: &tempfile::TempDir,
    run: RunId,
) -> TestResult<ToolRuntime> {
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    Ok(ToolRuntime::new(
        registry,
        Arc::new(environment),
        Arc::new(ReadOnlyToolPolicy),
    ))
}

fn request(ids: TestIds, tool: &str, arguments: &str) -> TestResult<ToolInvocationRequest> {
    Ok(ToolInvocationRequest::new(
        ids.run,
        ids.turn,
        ids.attempt,
        ids.call,
        ToolName::new(tool)?,
        ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::Direct)?,
        Arc::<str>::from(arguments),
    ))
}

fn control(ids: TestIds, cancellation: CancellationToken) -> TestResult<ToolExecutionControl> {
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(Timestamp::new(71, 0)?));
    let (progress, _receiver) = ProgressSink::channel(32, ids.call, Arc::clone(&clock))?;
    Ok(ToolExecutionControl::new(
        cancellation,
        None,
        clock,
        progress,
    ))
}

async fn execute_prepared_read_only(
    runtime: &ToolRuntime,
    invocation: ToolInvocation,
    control: ToolExecutionControl,
) -> TestResult<ToolOutcome> {
    let EffectAuthorization::Allowed(authorized) = runtime.authorize(invocation).await? else {
        return Err("standard read-only tool was not authorized".into());
    };
    Ok(runtime.execute(authorized, control).await?)
}

#[derive(Clone)]
struct AllowPolicy;

impl ToolPolicy for AllowPolicy {
    fn revision(&self) -> ToolPolicyRevision {
        match ToolPolicyRevision::new("test.standard-coding", 1) {
            Ok(revision) => revision,
            Err(error) => panic!("static policy revision failed: {error}"),
        }
    }

    fn evaluate(&self, _request: plakous_tools::ToolPolicyRequest) -> ToolPolicyFuture {
        Box::pin(std::future::ready(Ok(ToolPolicyDecision::Allowed)))
    }
}

fn claim_evidence() -> TestResult<Digest> {
    Ok(Digest::sha256(
        DigestDomain::Snapshot,
        1,
        &[b"test/durable-effect-claim"],
    )?)
}

fn execute_effect<'a>(
    runtime: &'a ToolRuntime,
    request: ToolInvocationRequest,
    control: ToolExecutionControl,
) -> Pin<Box<dyn Future<Output = TestResult<ToolOutcome>> + 'a>> {
    Box::pin(async move {
        let invocation = runtime.prepare(request)?;
        let EffectAuthorization::Allowed(authorized) = runtime.authorize(invocation).await? else {
            return Err("standard effectful tool was not authorized".into());
        };
        let intent = runtime.effect_dispatch_intent(&authorized)?;
        let claimed = runtime.claim_effect(authorized, intent, claim_evidence()?)?;
        Ok(Box::pin(runtime.execute_claimed(claimed, control))
            .await
            .into_result()?)
    })
}

fn text(outcome: &ToolOutcome) -> TestResult<&str> {
    match outcome.parts().first() {
        Some(ToolResultPart::Text(text)) => Ok(text.text()),
        _ => Err("standard tool outcome omitted its text part".into()),
    }
}

#[tokio::test]
async fn both_synthetic_maps_dispatch_exact_standard_executors() -> TestResult {
    let workspace = tempfile::tempdir()?;
    fs::write(workspace.path().join("source.txt"), b"alpha\n")?;
    let ids = ids()?;
    let registry = registry()?;
    let read = registry
        .identity(&ToolName::new("read_file")?)
        .cloned()
        .ok_or("read identity missing")?;
    let search = registry
        .identity(&ToolName::new("search_repository")?)
        .cloned()
        .ok_or("search identity missing")?;
    let codex = ToolExposureMap::new(registry.clone(), [ToolExposure::new(search, "grep_files")?])?;
    let claude = ToolExposureMap::new(registry.clone(), [ToolExposure::new(read, "Read")?])?;
    let runtime = runtime(registry, &workspace, ids.run)?;

    let read_invocation = claude.prepare(request(ids, "Read", r#"{"path":"source.txt"}"#)?)?;
    let read = execute_prepared_read_only(
        &runtime,
        read_invocation,
        control(ids, CancellationToken::new())?,
    )
    .await?;
    assert!(text(&read)?.contains("alpha"));

    let search_ids = with_call(ids, 0x65)?;
    let search_invocation = codex.prepare(request(
        search_ids,
        "grep_files",
        r#"{"query":"alpha","path":"."}"#,
    )?)?;
    let search = execute_prepared_read_only(
        &runtime,
        search_invocation,
        control(search_ids, CancellationToken::new())?,
    )
    .await?;
    assert!(text(&search)?.contains("source.txt:1:1:alpha"));
    Ok(())
}
#[tokio::test]
async fn patch_and_plan_use_durably_claimed_mutating_dispatch() -> TestResult {
    let workspace = tempfile::tempdir()?;
    fs::write(workspace.path().join("old.txt"), b"before\n")?;
    let ids = ids()?;
    let plan = Arc::new(InMemoryPlanState::new());
    let registry = registry_with_plan(plan.clone())?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let runtime = ToolRuntime::new(registry, Arc::new(environment), Arc::new(AllowPolicy));

    let patch_text = "diff --git a/old.txt b/old.txt\n--- a/old.txt\n+++ b/old.txt\n@@ -1 +1 @@\n-before\n+after\ndiff --git a/new.txt b/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+created\n";
    let patch_args = serde_json::json!({"patch": patch_text});
    let patch = execute_effect(
        &runtime,
        request(ids, "apply_patch", &patch_args.to_string())?,
        control(ids, CancellationToken::new())?,
    )
    .await?;
    assert_eq!(patch.kind(), ToolOutcomeKind::Success);
    assert_eq!(fs::read(workspace.path().join("old.txt"))?, b"after\n");
    assert_eq!(fs::read(workspace.path().join("new.txt"))?, b"created\n");

    let conflict_ids = with_call(ids, 0x65)?;
    let conflict_text = "diff --git a/old.txt b/old.txt\n--- a/old.txt\n+++ b/old.txt\n@@ -1 +1 @@\n-stale\n+wrong\n";
    let conflict_args = serde_json::json!({"patch": conflict_text});
    let conflict = execute_effect(
        &runtime,
        request(conflict_ids, "apply_patch", &conflict_args.to_string())?,
        control(conflict_ids, CancellationToken::new())?,
    )
    .await?;
    assert_eq!(conflict.kind(), ToolOutcomeKind::Failure);
    assert_eq!(fs::read(workspace.path().join("old.txt"))?, b"after\n");

    let plan_ids = with_call(ids, 0x67)?;
    let update = execute_effect(
        &runtime,
        request(
            plan_ids,
            "update_plan",
            r#"{"plan":[{"step":"implement","status":"in_progress"},{"step":"verify","status":"pending"}]}"#,
        )?,
        control(plan_ids, CancellationToken::new())?,
    )
    .await?;
    assert_eq!(update.kind(), ToolOutcomeKind::Success);
    assert_eq!(plan.snapshot()?.revision(), 1);
    assert_eq!(plan.snapshot()?.items().len(), 2);
    Ok(())
}
