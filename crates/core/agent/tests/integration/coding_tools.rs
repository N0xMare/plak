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
    execute_effect_with_value(runtime, request, control, false)
}

fn execute_effect_with_value<'a>(
    runtime: &'a ToolRuntime,
    request: ToolInvocationRequest,
    control: ToolExecutionControl,
    want_value: bool,
) -> Pin<Box<dyn Future<Output = TestResult<ToolOutcome>> + 'a>> {
    Box::pin(async move {
        let invocation = runtime.prepare(request)?.with_want_value(want_value);
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
async fn typed_reads_searches_and_listings_are_demand_driven_and_keep_exact_text() -> TestResult {
    use plakous_tools::{ExecutionValue, ListedEntryKind, WorkspaceContentIdentity};

    let workspace = tempfile::tempdir()?;
    let content = b"alpha\nbeta\nalpha again\n";
    fs::write(workspace.path().join("source.txt"), content)?;
    let ids = ids()?;
    let runtime = runtime(registry()?, &workspace, ids.run)?;
    for (index, (name, args)) in [
        ("read_file", r#"{"path":"source.txt","offset":2,"limit":1}"#),
        (
            "search_repository",
            r#"{"path":".","query":"alpha","head_limit":1}"#,
        ),
        (
            "search_repository",
            r#"{"path":".","query":"alpha","use_regex":true,"head_limit":1}"#,
        ),
        (
            "search_repository",
            r#"{"path":".","query":"","list_directory":true}"#,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let plain_ids = with_call(ids, 0x70 + u8::try_from(index)? * 2)?;
        let typed_ids = with_call(ids, 0x71 + u8::try_from(index)? * 2)?;
        let invocation = runtime.prepare(request(plain_ids, name, args)?)?;
        assert!(!invocation.want_value());
        let mut plain = Box::pin(execute_prepared_read_only(
            &runtime,
            invocation,
            control(plain_ids, CancellationToken::new())?,
        ))
        .await?;
        assert!(plain.take_value().is_none());
        let mut typed = Box::pin(execute_prepared_read_only(
            &runtime,
            runtime
                .prepare(request(typed_ids, name, args)?)?
                .with_want_value(true),
            control(typed_ids, CancellationToken::new())?,
        ))
        .await?;
        assert_eq!(typed.parts(), plain.parts());
        assert_eq!(typed.retained_bytes(), plain.retained_bytes());
        match typed.take_value().ok_or("demanded value absent")? {
            ExecutionValue::FileRead(value) => {
                assert_eq!(value.path, "source.txt");
                assert_eq!(value.text, "beta\n");
                assert_eq!(value.first_line, 2);
                assert_eq!(value.lines, 1);
                assert!(!value.eof);
                assert_eq!(
                    value.revision,
                    Some(WorkspaceContentIdentity::from_bytes(content)?)
                );
            }
            ExecutionValue::SearchHits(value) => {
                assert_eq!(value.hits.len(), 1);
                assert_eq!(value.hits[0].text, "alpha");
                assert_eq!(value.hits[0].line, 1);
                if args.contains("use_regex") {
                    assert_eq!(value.hits_cut, 0);
                    assert!(value.incomplete);
                } else {
                    assert_eq!(value.hits_cut, 1);
                }
            }
            ExecutionValue::Listing(value) => {
                assert_eq!(value.entries.len(), 1);
                assert_eq!(value.entries[0].path, "source.txt");
                assert_eq!(value.entries[0].kind, ListedEntryKind::File);
                assert_eq!(value.entries[0].size, content.len() as u64);
                assert!(!value.incomplete);
            }
            _ => return Err("unexpected value kind".into()),
        }
        assert!(typed.take_value().is_none());
    }
    Ok(())
}

#[tokio::test]
async fn typed_mutations_report_committed_revisions_without_changing_text() -> TestResult {
    use plakous_tools::{ExecutionValue, FileChangeAction, WorkspaceContentIdentity};

    let workspace = tempfile::tempdir()?;
    let ids = ids()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let environment = Arc::new(environment);
    let mut plan_texts = Vec::new();
    for want_value in [false, true] {
        let runtime =
            ToolRuntime::new(registry()?, Arc::clone(&environment), Arc::new(AllowPolicy));
        let patch_ids = with_call(ids, if want_value { 0x72 } else { 0x70 })?;
        let plan_ids = with_call(ids, if want_value { 0x73 } else { 0x71 })?;
        fs::write(workspace.path().join("old.txt"), b"before\n")?;
        let mut patch = execute_effect_with_value(
            &runtime,
            request(
                patch_ids,
                "apply_patch",
                r#"{"file_path":"old.txt","old_string":"before","new_string":"after"}"#,
            )?,
            control(patch_ids, CancellationToken::new())?,
            want_value,
        )
        .await?;
        assert_eq!(patch.joined_text(), "changed_files=1");
        match patch.take_value() {
            Some(ExecutionValue::FileChanges(value)) if want_value => {
                assert_eq!(value.changes.len(), 1);
                assert_eq!(value.changes[0].path, "old.txt");
                assert_eq!(value.changes[0].action, FileChangeAction::Update);
                assert_eq!(
                    value.changes[0].revision,
                    Some(WorkspaceContentIdentity::from_bytes(b"after\n")?)
                );
            }
            None if !want_value => (),
            _ => return Err("unexpected patch value".into()),
        }
        let mut plan = execute_effect_with_value(
            &runtime,
            request(
                plan_ids,
                "update_plan",
                r#"{"plan":[{"step":"work","status":"pending"}]}"#,
            )?,
            control(plan_ids, CancellationToken::new())?,
            want_value,
        )
        .await?;
        plan_texts.push(plan.joined_text());
        match plan.take_value() {
            Some(ExecutionValue::Plan(value)) if want_value => assert_eq!(value.revision, 1),
            None if !want_value => (),
            _ => return Err("unexpected plan value".into()),
        }
    }
    assert_eq!(plan_texts[0], plan_texts[1]);
    Ok(())
}

#[tokio::test]
async fn demanded_large_search_values_are_bounded_with_real_omission_counts() -> TestResult {
    use plakous_tools::ExecutionValue;
    let workspace = tempfile::tempdir()?;
    let line = format!("alpha{}\n", "x".repeat(3000));
    for path in ["one.txt", "two.txt"] {
        fs::write(workspace.path().join(path), line.repeat(200))?;
    }
    let ids = ids()?;
    let runtime = runtime(registry()?, &workspace, ids.run)?;
    let args =
        r#"{"path":".","query":"alpha","use_regex":true,"counts_only":true,"head_limit":4096}"#;
    let invocation = runtime.prepare(request(ids, "search_repository", args)?)?;
    let plain = Box::pin(execute_prepared_read_only(
        &runtime,
        invocation,
        control(ids, CancellationToken::new())?,
    ))
    .await?;
    let typed_ids = with_call(ids, 0x70)?;
    let mut typed = Box::pin(execute_prepared_read_only(
        &runtime,
        runtime
            .prepare(request(typed_ids, "search_repository", args)?)?
            .with_want_value(true),
        control(typed_ids, CancellationToken::new())?,
    ))
    .await?;
    assert_eq!(plain.parts(), typed.parts());
    let value = typed.take_value().ok_or("large search value absent")?;
    let ExecutionValue::SearchHits(value) = value else {
        return Err("search returned another value".into());
    };
    assert_ne!(value.hits, [] as [plakous_tools::SearchHit; 0]);
    assert!(
        value.hits.len() > 300,
        "use actual UTF-8 storage, not a threefold estimate"
    );
    assert!(value.hits_cut > 0);
    assert_eq!(value.hits.len() as u64 + value.hits_cut, 400);
    assert!(!value.incomplete);
    Ok(())
}

#[tokio::test]
async fn regex_values_follow_count_path_and_context_windows() -> TestResult {
    use plakous_tools::ExecutionValue;
    let workspace = tempfile::tempdir()?;
    fs::write(workspace.path().join("a.txt"), "alpha\nalpha\n")?;
    fs::write(workspace.path().join("b.txt"), "alpha\nalpha\nalpha\n")?;
    fs::write(
        workspace.path().join("context.txt"),
        "before\nalpha\nmiddle\nalpha\nend\n",
    )?;
    let ids = ids()?;
    let runtime = runtime(registry()?, &workspace, ids.run)?;
    for (index, (args, paths, lines, cut)) in [
        (r#"{"path":".","query":"alpha","use_regex":true,"counts_only":true,"offset":1,"head_limit":1}"#,
            vec!["b.txt"; 3], vec![1, 2, 3], 4),
        (r#"{"path":".","query":"alpha","use_regex":true,"paths_only":true,"offset":1,"head_limit":1}"#,
            vec!["b.txt"; 3], vec![1, 2, 3], 4),
        (r#"{"path":"context.txt","query":"alpha","use_regex":true,"context":1,"offset":1,"head_limit":2}"#,
            vec!["context.txt"], vec![2], 1),
        (r#"{"path":"context.txt","query":"alpha","use_regex":true,"context":1,"offset":2,"head_limit":1}"#,
            vec![], vec![], 2),
    ].into_iter().enumerate() {
        let call_ids = with_call(ids, 0x80 + u8::try_from(index)?)?;
        let invocation = runtime.prepare(request(call_ids, "search_repository", args)?)?
            .with_want_value(true);
        let mut outcome = Box::pin(execute_prepared_read_only(
            &runtime, invocation, control(call_ids, CancellationToken::new())?,
        )).await?;
        let Some(ExecutionValue::SearchHits(value)) = outcome.take_value() else {
            return Err("missing regex hits".into());
        };
        assert_eq!(value.hits.iter().map(|hit| hit.path.as_str()).collect::<Vec<_>>(), paths);
        assert_eq!(value.hits.iter().map(|hit| hit.line).collect::<Vec<_>>(), lines);
        assert_eq!(value.hits_cut, cut);
        if index == 2 {
            assert!(outcome.joined_text().contains("context.txt:2:alpha\ncontext.txt-3-middle"));
        }
    }
    Ok(())
}

#[tokio::test]
async fn content_values_report_clipped_lines_and_glob_paths_have_no_value() -> TestResult {
    use plakous_tools::ExecutionValue;
    let workspace = tempfile::tempdir()?;
    fs::write(
        workspace.path().join("long.txt"),
        format!("alpha{}\n", "x".repeat(5000)),
    )?;
    let ids = ids()?;
    let runtime = runtime(registry()?, &workspace, ids.run)?;
    let invocation = runtime
        .prepare(request(
            ids,
            "search_repository",
            r#"{"path":".","query":"alpha"}"#,
        )?)?
        .with_want_value(true);
    let mut outcome = Box::pin(execute_prepared_read_only(
        &runtime,
        invocation,
        control(ids, CancellationToken::new())?,
    ))
    .await?;
    let Some(ExecutionValue::SearchHits(value)) = outcome.take_value() else {
        return Err("missing content hits".into());
    };
    assert_eq!(value.hits.len(), 1);
    assert!(value.hits[0].truncated);
    let call_ids = with_call(ids, 0x81)?;
    let invocation = runtime
        .prepare(request(
            call_ids,
            "search_repository",
            r#"{"path":".","query":"","paths_only":true,"filename_glob":"*.txt"}"#,
        )?)?
        .with_want_value(true);
    let mut outcome = Box::pin(execute_prepared_read_only(
        &runtime,
        invocation,
        control(call_ids, CancellationToken::new())?,
    ))
    .await?;
    assert!(outcome.joined_text().contains("long.txt"));
    assert!(outcome.take_value().is_none());
    Ok(())
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
    let read = Box::pin(execute_prepared_read_only(
        &runtime,
        read_invocation,
        control(ids, CancellationToken::new())?,
    ))
    .await?;
    assert!(text(&read)?.contains("alpha"));

    let search_ids = with_call(ids, 0x65)?;
    let search_invocation = codex.prepare(request(
        search_ids,
        "grep_files",
        r#"{"query":"alpha","path":"."}"#,
    )?)?;
    let search = Box::pin(execute_prepared_read_only(
        &runtime,
        search_invocation,
        control(search_ids, CancellationToken::new())?,
    ))
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
