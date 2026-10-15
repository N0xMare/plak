//! Portable coding-pack contracts, exercised without the agent runtime.

#![cfg_attr(
    not(unix),
    allow(
        dead_code,
        unused_imports,
        reason = "Unix host filesystem coding-tool tests are cfg-gated"
    )
)]

use std::fmt::Write as _;
use std::fs;
use std::str::FromStr;
use std::sync::Arc;

use plakous_model::{
    AttemptId, CallId, CancellationToken, Clock, Digest, DigestDomain, FixedClock, RunId,
    Timestamp, ToolCorrelation, ToolEffect, ToolInputContract, ToolName, ToolResultPart, TurnId,
};
use plakous_tool_plan::{InMemoryPlanState, PlanItem, PlanItemStatus, PlanState, PlanToolPack};
use plakous_tools::{
    DeterministicExecutionBackend, DeterministicProcessPolicy, EffectAuthorizationGrantSource,
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    MAX_STANDARD_IMAGE_BYTES, MAX_STANDARD_PLAN_STEP_BYTES, ProgressSink, ToolInvocationRequest,
    ToolOutcome, ToolOutcomeKind, ToolPack, ToolPolicyRevision, ToolRegistryBuilder,
    ToolRuntimeErrorKind, WorkspacePolicy,
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Copy)]
struct Ids {
    run: RunId,
    turn: TurnId,
    attempt: AttemptId,
    call: CallId,
}

#[test]
fn read_file_rejects_invented_digest_continuation_objects() -> TestResult {
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let definition = registry
        .definition(&ToolName::new("read_file")?)
        .ok_or("read_file definition missing")?;
    let ToolInputContract::Structured(schema) = definition.input() else {
        return Err("read_file input is not structured".into());
    };
    let properties = &schema.document().value()["properties"];
    assert!(properties.get("continuation").is_none());
    assert!(properties.get("content_sha256").is_none());
    assert!(properties.get("content_bytes").is_none());
    assert!(properties.get("offset").is_some());
    assert!(properties.get("limit").is_some());
    assert!(schema.parse_instance(r#"{"path":"src/lib.rs"}"#).is_ok());
    assert!(
        schema
            .parse_instance(r#"{"path":"src/lib.rs","offset":1,"limit":20}"#)
            .is_ok()
    );
    assert!(
        schema
            .parse_instance(r#"{"path":"src/lib.rs","offset":0}"#)
            .is_err()
    );
    assert!(
        schema
            .parse_instance(
                r#"{"path":"src/lib.rs","continuation":{"offset":1,"content_bytes":2,"content_sha256":"00"}}"#,
            )
            .is_err()
    );
    Ok(())
}

fn ids(call: u8) -> TestResult<Ids> {
    Ok(Ids {
        run: RunId::from_str("00000000-0000-0000-0000-000000000061")?,
        turn: TurnId::from_str("00000000-0000-0000-0000-000000000062")?,
        attempt: AttemptId::from_str("00000000-0000-0000-0000-000000000063")?,
        call: CallId::from_str(&format!("00000000-0000-0000-0000-{call:012x}"))?,
    })
}

fn registry(plan: Arc<dyn PlanState>) -> TestResult<plakous_tools::FrozenToolRegistry> {
    let packs: Vec<Arc<dyn ToolPack>> = vec![
        Arc::new(plakous_tool_files::FilesToolPack),
        Arc::new(plakous_tool_search::SearchToolPack),
        Arc::new(PlanToolPack::new(plan)),
    ];
    let mut builder = ToolRegistryBuilder::default();
    plakous_tools::register_packs(&packs, &mut builder)?;
    Ok(builder.freeze())
}

fn text(outcome: &ToolOutcome) -> TestResult<&str> {
    match outcome.parts().first() {
        Some(ToolResultPart::Text(text)) => Ok(text.text()),
        _ => Err("coding tool omitted text output".into()),
    }
}

async fn execute(
    registry: &plakous_tools::FrozenToolRegistry,
    environment: Arc<ExecutionEnvironment>,
    ids: Ids,
    tool: &str,
    arguments: &str,
) -> TestResult<ToolOutcome> {
    execute_with_cancellation(
        registry,
        environment,
        ids,
        tool,
        arguments,
        CancellationToken::new(),
    )
    .await
}

async fn execute_with_cancellation(
    registry: &plakous_tools::FrozenToolRegistry,
    environment: Arc<ExecutionEnvironment>,
    ids: Ids,
    tool: &str,
    arguments: &str,
    cancellation: CancellationToken,
) -> TestResult<ToolOutcome> {
    let invocation = registry.prepare(ToolInvocationRequest::new(
        ids.run,
        ids.turn,
        ids.attempt,
        ids.call,
        ToolName::new(tool)?,
        ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::default())?,
        Arc::<str>::from(arguments),
    ))?;
    execute_invocation(registry, environment, ids, invocation, cancellation).await
}

async fn execute_invocation(
    registry: &plakous_tools::FrozenToolRegistry,
    environment: Arc<ExecutionEnvironment>,
    ids: Ids,
    mut invocation: plakous_tools::ToolInvocation,
    cancellation: CancellationToken,
) -> TestResult<ToolOutcome> {
    let effect = registry
        .definition(invocation.identity().name())
        .ok_or("missing definition")?
        .effect();
    if effect == ToolEffect::Mutating {
        let intent = Digest::sha256(DigestDomain::Snapshot, 1, &[b"portable-test-intent"])?;
        let claim = Digest::sha256(DigestDomain::Snapshot, 1, &[b"portable-test-claim"])?;
        invocation = plakous_tools::internal::bind_effect_dispatch(invocation, intent, claim);
    }
    let revision = ToolPolicyRevision::new("portable-coding-test", 1)?;
    let binding = plakous_tools::internal::authorization_binding(
        &invocation,
        environment.descriptor().clone(),
        revision,
    )?;
    let grant = plakous_tools::internal::authorization_grant(
        binding,
        EffectAuthorizationGrantSource::Policy,
    );
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(Timestamp::new(71, 0)?));
    let (progress, _receiver) = ProgressSink::channel(8, ids.call, Arc::clone(&clock))?;
    let context = plakous_tools::internal::tool_context(
        ids.run,
        ids.turn,
        ids.attempt,
        cancellation,
        None,
        clock,
        environment,
        grant,
        progress,
    );
    let executor = plakous_tools::internal::executor(registry, &invocation)?;
    Ok(executor.execute(invocation, context).await?)
}

#[test]
fn coding_pack_registry_schema_and_descriptor_are_portable() -> TestResult {
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    // Three stateless capabilities, the Codex patch envelope over the same
    // apply-patch executor, one search, and the plan port's two.
    assert_eq!(registry.definitions().len(), 7);
    assert_eq!(
        registry
            .definitions()
            .iter()
            .filter(|definition| definition.effect() == ToolEffect::ReadOnly)
            .count(),
        3
    );
    let patch = registry
        .definition(&ToolName::new("apply_patch")?)
        .ok_or("patch definition missing")?;
    assert!(patch.description().contains("unified diff"));
    assert!(patch.description().contains("Codex *** Begin Patch"));
    assert!(PlanItem::new("é", PlanItemStatus::Pending).is_ok());
    let encoded = serde_json::to_string(&PlanItem::new("verify", PlanItemStatus::Completed)?)?;
    let decoded: PlanItem = serde_json::from_str(&encoded)?;
    assert_eq!(decoded.step(), "verify");
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn read_file_paths_resolve_from_workspace_root_not_cwd() -> TestResult {
    let workspace = tempfile::tempdir()?;
    fs::write(
        workspace.path().join("Cargo.toml"),
        b"[package]\nname = \"demo\"\n",
    )?;
    fs::create_dir(workspace.path().join("src"))?;
    fs::write(workspace.path().join("src/lib.rs"), b"pub fn lib() {}\n")?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x72)?.run),
        workspace.path(),
        "src",
        ExecutionEnvironmentConfig::default(),
    )?);

    let root_manifest = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x72)?,
        "read_file",
        r#"{"path":"Cargo.toml"}"#,
    )
    .await?;
    assert!(text(&root_manifest)?.contains("[package]"));

    let cwd_relative = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x73)?,
        "read_file",
        r#"{"path":"lib.rs"}"#,
    )
    .await;
    assert_eq!(
        runtime_error_kind(
            require_error(
                cwd_relative,
                "read_file unexpectedly resolved lib.rs from session cwd"
            )?
            .as_ref()
        ),
        Some(ToolRuntimeErrorKind::WorkspacePolicy)
    );

    let nested_source = execute(
        &registry,
        environment,
        ids(0x74)?,
        "read_file",
        r#"{"path":"src/lib.rs"}"#,
    )
    .await?;
    assert!(text(&nested_source)?.contains("pub fn lib"));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn search_repository_head_limit_survives_semantic_json_floats() -> TestResult {
    let workspace = tempfile::tempdir()?;
    fs::create_dir(workspace.path().join("src"))?;
    fs::write(
        workspace.path().join("src/lib.rs"),
        b"fn alpha() {}\nfn alpha_two() {}\nfn alpha_three() {}\n",
    )?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x75)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let capped = execute(
        &registry,
        environment,
        ids(0x75)?,
        "search_repository",
        r#"{"query":"alpha","path":"src","head_limit":1}"#,
    )
    .await?;
    let capped_text = text(&capped)?;
    assert!(
        capped_text.starts_with("matches=3 "),
        "head_limit must not change the match count header: {capped_text}"
    );
    assert_eq!(
        capped_text.lines().skip(1).count(),
        1,
        "semantic-JSON float head_limit must cap printed matches: {capped_text}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn portable_read_search_image_patch_process_and_plan_executors() -> TestResult {
    let workspace = tempfile::tempdir()?;
    fs::create_dir(workspace.path().join("src"))?;
    fs::write(workspace.path().join("src/lib.rs"), b"fn alpha() {}\n")?;
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&[0; 8]);
    png.extend_from_slice(&2_u32.to_be_bytes());
    png.extend_from_slice(&3_u32.to_be_bytes());
    png.extend_from_slice(b"bounded-test-image");
    fs::write(workspace.path().join("image.png"), png)?;
    fs::write(workspace.path().join("old.txt"), b"before\n")?;

    let plan = Arc::new(InMemoryPlanState::new());
    let registry = registry(plan.clone())?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x64)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);

    let read = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x64)?,
        "read_file",
        r#"{"path":"src/lib.rs"}"#,
    )
    .await?;
    assert!(text(&read)?.contains("fn alpha"));
    let search = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x65)?,
        "search_repository",
        r#"{"query":"alpha","path":"src"}"#,
    )
    .await?;
    assert!(text(&search)?.contains("src/lib.rs:1:4:fn alpha"));
    let image = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x66)?,
        "inspect_image",
        r#"{"path":"image.png"}"#,
    )
    .await?;
    assert_eq!(image.parts().len(), 2);

    let patch_args = serde_json::json!({"patch": "diff --git a/old.txt b/old.txt\n--- a/old.txt\n+++ b/old.txt\n@@ -1 +1 @@\n-before\n+after\n"});
    let patch = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x67)?,
        "apply_patch",
        &patch_args.to_string(),
    )
    .await?;
    assert_eq!(patch.kind(), ToolOutcomeKind::Success);
    assert_eq!(fs::read(workspace.path().join("old.txt"))?, b"after\n");

    let envelope = "*** Begin Patch\n*** Add File: hello.txt\n+Hello envelope\n*** End Patch\n";
    let created = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x68)?,
        "apply_patch",
        &serde_json::json!({"patch": envelope}).to_string(),
    )
    .await?;
    assert_eq!(created.kind(), ToolOutcomeKind::Success);
    assert_eq!(
        fs::read(workspace.path().join("hello.txt"))?,
        b"Hello envelope\n"
    );
    let replaced = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x6a)?,
        "apply_patch",
        r#"{"file_path":"hello.txt","old_string":"Hello envelope\n","new_string":"Hello replaced\n"}"#,
    )
    .await?;
    assert_eq!(replaced.kind(), ToolOutcomeKind::Success);
    assert_eq!(
        fs::read(workspace.path().join("hello.txt"))?,
        b"Hello replaced\n"
    );

    let update = execute(
        &registry,
        environment,
        ids(0x69)?,
        "update_plan",
        r#"{"plan":[{"step":"verify","status":"in_progress"}]}"#,
    )
    .await?;
    assert_eq!(update.kind(), ToolOutcomeKind::Success);
    assert_eq!(plan.snapshot()?.items()[0].step(), "verify");
    Ok(())
}

#[tokio::test]
async fn portable_executor_enforces_environment_containment() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x70)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let result = execute(
        &registry,
        environment,
        ids(0x70)?,
        "read_file",
        r#"{"path":"../escape"}"#,
    )
    .await;
    let Err(error) = result else {
        return Err("workspace traversal unexpectedly succeeded".into());
    };
    assert_eq!(
        error
            .downcast_ref::<plakous_tools::ToolRuntimeError>()
            .map(plakous_tools::ToolRuntimeError::kind),
        Some(ToolRuntimeErrorKind::WorkspacePolicy)
    );
    Ok(())
}

#[tokio::test]
async fn portable_executor_accepts_absolute_paths_under_workspace_root() -> TestResult {
    let workspace = tempfile::tempdir()?;
    fs::write(workspace.path().join("inside.txt"), b"hello\n")?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x81)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let path = workspace.path().canonicalize()?.join("inside.txt");
    let args = serde_json::json!({"path": path});
    let read = execute(
        &registry,
        environment,
        ids(0x81)?,
        "read_file",
        &args.to_string(),
    )
    .await?;
    assert_eq!(read.kind(), ToolOutcomeKind::Success);
    assert!(text(&read)?.contains("hello"));
    Ok(())
}

#[tokio::test]
async fn apply_patch_preflight_open_failure_is_definite_tool_failure() -> TestResult {
    let workspace = tempfile::tempdir()?;
    fs::create_dir(workspace.path().join("not-a-file"))?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x82)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let patch_args = serde_json::json!({
        "patch": "diff --git a/not-a-file b/not-a-file\nnew file mode 100644\n--- /dev/null\n+++ b/not-a-file\n@@ -0,0 +1 @@\n+created\n"
    });
    let outcome = execute(
        &registry,
        environment,
        ids(0x82)?,
        "apply_patch",
        &patch_args.to_string(),
    )
    .await?;
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    Ok(())
}

/// A multi-edit call matches every edit against the original file and applies
/// all of them in one replacement, or none: a missing, repeated or overlapping
/// old text fails the whole call and leaves the file as it was.
#[tokio::test]
async fn apply_patch_edits_apply_together_or_not_at_all() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let original = b"alpha beta\ngamma delta\nbeta\n";
    fs::write(workspace.path().join("seed.txt"), original)?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x90)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    for (call, edits, refusal) in [
        (
            0x90,
            serde_json::json!([{"old_string": "zeta", "new_string": "z"}]),
            "edits[0]: old text was not found",
        ),
        (
            0x91,
            serde_json::json!([{"old_string": "beta", "new_string": "b"}]),
            "edits[0]: old text matched more than once",
        ),
        (
            0x92,
            serde_json::json!([
                {"old_string": "alpha beta", "new_string": "a"},
                {"old_string": "a beta\ngamma", "new_string": "g"}
            ]),
            "edits[0] and edits[1] overlap",
        ),
        (
            0x93,
            serde_json::json!([
                {"old_string": "gamma", "new_string": "G"},
                {"old_string": "zeta", "new_string": "z"}
            ]),
            "edits[1]: old text was not found",
        ),
    ] {
        let outcome = execute(
            &registry,
            Arc::clone(&environment),
            ids(call)?,
            "apply_patch",
            &serde_json::json!({"file_path": "seed.txt", "edits": edits}).to_string(),
        )
        .await?;
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure, "{refusal}");
        assert!(text(&outcome)?.contains(refusal), "{}", text(&outcome)?);
        assert_eq!(fs::read(workspace.path().join("seed.txt"))?, original);
    }
    let mixed = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x94)?,
        "apply_patch",
        r#"{"file_path":"seed.txt","old_string":"gamma","new_string":"G","edits":[{"old_string":"delta","new_string":"D"}]}"#,
    )
    .await?;
    assert_eq!(mixed.kind(), ToolOutcomeKind::Failure);
    let pathless = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x96)?,
        "apply_patch",
        r#"{"edits":[{"old_string":"delta","new_string":"D"}]}"#,
    )
    .await?;
    assert_eq!(pathless.kind(), ToolOutcomeKind::Failure);
    assert!(text(&pathless)?.contains("edits need file_path"));
    assert_eq!(fs::read(workspace.path().join("seed.txt"))?, original);
    // Each old text is matched in the original, so the second edit sees
    // `alpha`, not the first edit's replacement.
    let applied = execute(
        &registry,
        environment,
        ids(0x95)?,
        "apply_patch",
        &serde_json::json!({
            "file_path": "seed.txt",
            "edits": [
                {"old_string": "gamma delta", "new_string": "alpha"},
                {"old_string": "alpha beta", "new_string": "one"}
            ]
        })
        .to_string(),
    )
    .await?;
    assert_eq!(
        applied.kind(),
        ToolOutcomeKind::Success,
        "{}",
        text(&applied)?
    );
    assert_eq!(
        fs::read(workspace.path().join("seed.txt"))?,
        b"one\nalpha\nbeta\n"
    );
    Ok(())
}

#[tokio::test]
async fn apply_patch_duplicate_path_is_definite_tool_failure_without_mutation() -> TestResult {
    let workspace = tempfile::tempdir()?;
    fs::write(workspace.path().join("seed.txt"), b"before\n")?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x83)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    for (call, add_path) in [(0x83, "seed.txt"), (0x84, "./seed.txt")] {
        let envelope = format!(
            "*** Begin Patch\n*** Delete File: seed.txt\n*** Add File: {add_path}\n+after\n*** End Patch\n"
        );
        let outcome = execute(
            &registry,
            Arc::clone(&environment),
            ids(call)?,
            "apply_patch",
            &serde_json::json!({"patch": envelope}).to_string(),
        )
        .await?;
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        assert!(text(&outcome)?.contains("duplicate path"));
        assert_eq!(fs::read(workspace.path().join("seed.txt"))?, b"before\n");
    }
    Ok(())
}

#[tokio::test]
async fn apply_patch_codex_envelope_accepts_absolute_path_under_root() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x83)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let path = workspace.path().canonicalize()?.join("hello.txt");
    let envelope = format!(
        "*** Begin Patch\n*** Add File: {}\n+Hello envelope\n*** End Patch\n",
        path.display()
    );
    let created = execute(
        &registry,
        environment,
        ids(0x83)?,
        "apply_patch",
        &serde_json::json!({"patch": envelope}).to_string(),
    )
    .await?;
    assert_eq!(created.kind(), ToolOutcomeKind::Success);
    assert_eq!(
        fs::read(workspace.path().join("hello.txt"))?,
        b"Hello envelope\n"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one regression follows both exact V2 patch projections through prepare and file effects"
)]
async fn codex_projected_patch_preserves_raw_utf8_through_prepare_and_execution() -> TestResult {
    use plakous_compose::CodingPresentationExt as _;
    let mut builder = ToolRegistryBuilder::default();
    plakous_tools::register_packs(
        &plakous_compose::standard_coding_packs_in_memory()?,
        &mut builder,
    )?;
    let registry = builder.freeze();
    let effective = serde_json::from_value(serde_json::to_value(Digest::sha256(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        &[b"codex-projected-patch"],
    )?)?)?;
    let target = plakous_tools::ProviderModelProjectionIdentityV2::new(
        plakous_model::IntegrationClass::Standard,
        "openai.codex.responses",
        "openai.codex_subscription",
        "openai.codex.product-code-mode.v1",
        "responses-family",
        "codex-test",
        "codex-test",
        effective,
        1,
    )?;
    let direct = plakous_tools::CodingPresentation::Codex.projection_map(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        target.clone(),
    )?;
    let patch_entry = direct
        .by_exposed(&ToolName::new("apply_patch")?)
        .ok_or("Codex patch projection missing")?;
    let legacy_lowering = Digest::sha256(
        DigestDomain::ToolDefinition,
        2,
        &[
            b"plakous-tools/projection-program-v2",
            b"lowering",
            b"codex-envelope-to-apply-patch",
            patch_entry.defaults().digest().bytes(),
        ],
    )?;
    assert_ne!(patch_entry.lowering_identity(), legacy_lowering);
    let malformed_map = plakous_tools::CodingPresentation::Codex.projection_map(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        target.clone(),
    )?;
    let nested = plakous_tools::CodingPresentation::Codex.compile_nested_projection(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        target,
        &[ToolName::new("apply_patch")?],
    )?;
    let workspace = tempfile::tempdir()?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0xa1)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    for (map, call, path) in [(direct, 0xa1, "direct.txt"), (nested, 0xa2, "nested.txt")] {
        let ids = ids(call)?;
        let patch = format!("*** Begin Patch\n*** Add File: {path}\n+created\n*** End Patch\n");
        let request = ToolInvocationRequest::new(
            ids.run,
            ids.turn,
            ids.attempt,
            ids.call,
            ToolName::new("apply_patch")?,
            ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::default())?,
            patch.clone(),
        );
        let (invocation, receipt) = map.prepare(request)?.into_parts();
        assert_eq!(
            invocation.arguments().value().as_str(),
            Some(patch.as_str())
        );
        let outcome = execute_invocation(
            &registry,
            Arc::clone(&environment),
            ids,
            invocation,
            CancellationToken::new(),
        )
        .await?;
        assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
        assert!(receipt.is_some());
        assert_eq!(fs::read(workspace.path().join(path))?, b"created\n");
    }

    let ids = ids(0xa3)?;
    let malformed = "*** Begin Patch\n*** Add File: rejected.txt\n+missing end\n";
    let request = ToolInvocationRequest::new(
        ids.run,
        ids.turn,
        ids.attempt,
        ids.call,
        ToolName::new("apply_patch")?,
        ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::default())?,
        malformed,
    );
    let (invocation, _) = malformed_map.prepare(request)?.into_parts();
    let outcome = execute_invocation(
        &registry,
        environment,
        ids,
        invocation,
        CancellationToken::new(),
    )
    .await?;
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    assert!(!workspace.path().join("rejected.txt").exists());
    Ok(())
}

#[tokio::test]
async fn apply_patch_escape_path_is_definite_tool_failure() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x84)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let envelope = "*** Begin Patch\n*** Add File: /etc/passwd\n+nope\n*** End Patch\n";
    let outcome = execute(
        &registry,
        environment,
        ids(0x84)?,
        "apply_patch",
        &serde_json::json!({"patch": envelope}).to_string(),
    )
    .await?;
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn search_list_dir_accepts_absolute_path_under_workspace_root() -> TestResult {
    let workspace = tempfile::tempdir()?;
    fs::write(workspace.path().join("inside.txt"), b"hello\n")?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x85)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let path = workspace.path().canonicalize()?;
    let listed = execute(
        &registry,
        environment,
        ids(0x85)?,
        "search_repository",
        &serde_json::json!({"path": path, "query": "", "list_directory": true}).to_string(),
    )
    .await?;
    assert_eq!(listed.kind(), ToolOutcomeKind::Success);
    assert!(text(&listed)?.contains("inside.txt"));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn search_repository_accepts_absolute_path_under_workspace_root() -> TestResult {
    let workspace = tempfile::tempdir()?;
    fs::create_dir(workspace.path().join("src"))?;
    fs::write(workspace.path().join("src/lib.rs"), b"fn alpha() {}\n")?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x86)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let path = workspace.path().canonicalize()?;
    let found = execute(
        &registry,
        environment,
        ids(0x86)?,
        "search_repository",
        &serde_json::json!({"query": "alpha", "path": path}).to_string(),
    )
    .await?;
    assert_eq!(found.kind(), ToolOutcomeKind::Success);
    assert!(text(&found)?.contains("alpha"));
    Ok(())
}

fn runtime_error_kind(error: &(dyn std::error::Error + 'static)) -> Option<ToolRuntimeErrorKind> {
    error
        .downcast_ref::<plakous_tools::ToolRuntimeError>()
        .map(plakous_tools::ToolRuntimeError::kind)
}

fn require_error(
    result: TestResult<ToolOutcome>,
    message: &'static str,
) -> TestResult<Box<dyn std::error::Error>> {
    match result {
        Err(error) => Ok(error),
        Ok(_) => Err(message.into()),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn portable_text_search_and_image_limits_fail_closed() -> TestResult {
    let workspace = tempfile::tempdir()?;
    fs::write(workspace.path().join("a.txt"), b"needle one\nneedle two\n")?;
    fs::write(workspace.path().join("b.txt"), b"needle three\n")?;
    fs::write(workspace.path().join("empty.txt"), b"")?;
    fs::write(workspace.path().join("binary.txt"), [0xff, 0xfe])?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x71)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);

    let empty = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x71)?,
        "read_file",
        r#"{"path":"empty.txt"}"#,
    )
    .await?;
    let empty_text = text(&empty)?;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(empty_text)?["content"],
        ""
    );
    let binary = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x72)?,
        "read_file",
        r#"{"path":"binary.txt"}"#,
    )
    .await;
    // Invalid UTF-8 is read lossily and says so; a file with NUL bytes is
    // refused as binary.
    let lossy: plakous_tools::WorkspaceFileReadResult = serde_json::from_str(text(&binary?)?)?;
    assert_eq!(lossy.content, "\u{fffd}\u{fffd}");
    assert!(lossy.lossy);
    fs::write(workspace.path().join("nul.bin"), b"ab\0cd")?;
    let nul = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x72)?,
        "read_file",
        r#"{"path":"nul.bin"}"#,
    )
    .await?;
    assert_eq!(nul.kind(), ToolOutcomeKind::Failure);
    assert!(text(&nul)?.contains("binary"));
    fs::remove_file(workspace.path().join("nul.bin"))?;
    let search = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x73)?,
        "search_repository",
        r#"{"query":"needle","path":"."}"#,
    )
    .await?;
    let searched = text(&search)?;
    assert!(
        searched
            .lines()
            .nth(1)
            .is_some_and(|line| line.starts_with("matches=3 files_considered=4 files_scanned=4 "))
    );
    assert!(searched.contains("skipped_binary=1 skipped_oversize=0 incomplete=false\n"));
    assert!(searched.contains("a.txt:1:1:needle one\n"));
    assert!(searched.contains("needle two"));
    assert!(searched.contains("needle three"));
    let mut oversized = b"GIF89a".to_vec();
    oversized.extend_from_slice(&1_u16.to_le_bytes());
    oversized.extend_from_slice(&1_u16.to_le_bytes());
    oversized.resize(
        usize::try_from(MAX_STANDARD_IMAGE_BYTES)?.saturating_add(1),
        0,
    );
    fs::write(workspace.path().join("large.gif"), oversized)?;
    let image = execute(
        &registry,
        environment,
        ids(0x74)?,
        "inspect_image",
        r#"{"path":"large.gif"}"#,
    )
    .await?;
    assert_eq!(image.kind(), ToolOutcomeKind::Failure);
    assert!(text(&image)?.contains("exceeds"));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn repository_search_model_visible_bytes_are_exactly_bounded_and_utf8_safe() -> TestResult {
    let workspace = tempfile::tempdir()?;
    for index in 0..40 {
        let line = format!("needle-{}\n", "€".repeat(1_500));
        fs::write(
            workspace.path().join(format!("résultat-{index:03}.txt")),
            line,
        )?;
    }
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x7d)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let outcome = execute(
        &registry,
        environment,
        ids(0x7d)?,
        "search_repository",
        r#"{"query":"needle","path":"."}"#,
    )
    .await?;
    let visible = text(&outcome)?;
    assert!(visible.len() <= 64 * 1024);
    assert!(
        visible
            .lines()
            .next()
            .is_some_and(|line| line.contains("incomplete=true"))
    );
    assert!(visible.contains("résultat-"));
    assert!(visible.contains('€'));
    Ok(())
}

/// Promotes one job printing two lines in an environment bound to a jobs root
/// under `state`, returning the service, the job number and its issued path.
#[cfg(unix)]
async fn issued_job_output(
    environment: &ExecutionEnvironment,
    state: &std::path::Path,
) -> TestResult<(plakous_tools::JoinedProcessSessions, u32, String)> {
    let sessions = environment.joined_process_sessions()?;
    sessions.bind_job_outputs(
        plakous_tools::JobOutputRoot::open(&state.join("jobs"))?,
        plakous_model::SessionId::from_str("00000000-0000-0000-0000-0000000000b0")?,
        plakous_model::BranchId::from_str("00000000-0000-0000-0000-0000000000c0")?,
    );
    let id = sessions.start(
        plakous_tools::JoinedProcessStartRequest::new(
            plakous_tools::JoinedProcessCommand::shell(
                plakous_tools::JoinedProcessShellProfile::new("/bin/sh", ["-c".to_owned()])?,
                "printf 'first\\nsecond\\n'; read line",
            )?,
            ".",
            Vec::new(),
            true,
            plakous_tools::JoinedProcessTerminalMode::Pipes,
        )?,
        plakous_tools::ExecutionOperationControl::new(CancellationToken::new()),
    )?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while sessions
        .poll_from(
            id,
            plakous_tools::JoinedProcessOutputCursor::new(0),
            4096,
            256,
        )?
        .records()
        .iter()
        .map(|record| record.bytes().len())
        .sum::<usize>()
        < "first\nsecond\n".len()
    {
        assert!(std::time::Instant::now() < deadline, "no output");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    plakous_tools::internal::promote(&sessions, id)?;
    let number = plakous_tools::JoinedProcessSessions::job_number(id);
    let path = sessions
        .job_output_file(number)
        .ok_or("promotion issued no output file")?
        .path;
    Ok((sessions, number, path))
}

#[cfg(unix)]
#[tokio::test]
async fn read_file_admits_exactly_an_issued_job_output_file() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x7c)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let (sessions, number, path) = issued_job_output(&environment, state.path()).await?;
    assert!(!path.starts_with(&workspace.path().display().to_string()));

    let read = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x7c)?,
        "read_file",
        &serde_json::json!({"path": path, "offset": 2}).to_string(),
    )
    .await?;
    let page: serde_json::Value = serde_json::from_str(text(&read)?)?;
    assert_eq!(page["content"], "second\n");
    assert_eq!(page["offset"], 2);

    // A sibling of the issued file is refused without being opened.
    let sibling = path.replace(&format!("-p{number}-"), "-p999-");
    fs::write(&sibling, b"sibling")?;
    let refused = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x7d)?,
        "read_file",
        &serde_json::json!({"path": sibling}).to_string(),
    )
    .await;
    let Err(error) = refused else {
        return Err("a sibling of an issued output file was read".into());
    };
    assert_eq!(
        runtime_error_kind(error.as_ref()),
        Some(ToolRuntimeErrorKind::WorkspacePolicy)
    );
    assert!(
        error
            .to_string()
            .contains("was issued by an earlier process or another agent"),
        "{error}"
    );

    // A relinked issued file is a policy refusal, not an execution failure.
    let link = std::path::Path::new(&path).with_file_name("link");
    fs::hard_link(&path, &link)?;
    let relinked = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x7f)?,
        "read_file",
        &serde_json::json!({"path": path}).to_string(),
    )
    .await;
    let Err(error) = relinked else {
        return Err("a relinked output file was read".into());
    };
    assert_eq!(
        runtime_error_kind(error.as_ref()),
        Some(ToolRuntimeErrorKind::WorkspacePolicy)
    );
    assert!(
        error.to_string().contains("was replaced, relinked"),
        "{error}"
    );
    fs::remove_file(&link)?;

    // A deleted issued file is a typed not-found naming the path.
    fs::remove_file(&path)?;
    let missing = execute(
        &registry,
        environment,
        ids(0x7e)?,
        "read_file",
        &serde_json::json!({"path": path}).to_string(),
    )
    .await;
    let Err(error) = missing else {
        return Err("a deleted output file was read".into());
    };
    assert_eq!(
        runtime_error_kind(error.as_ref()),
        Some(ToolRuntimeErrorKind::WorkspacePolicy)
    );
    assert!(
        error
            .to_string()
            .contains(&format!("job output file {path} no longer exists")),
        "{error}"
    );
    sessions.join_all()?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn read_file_pages_deep_into_a_multi_slice_job_output_file() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x70)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let sessions = environment.joined_process_sessions()?;
    sessions.bind_job_outputs(
        plakous_tools::JobOutputRoot::open(&state.path().join("jobs"))?,
        plakous_model::SessionId::from_str("00000000-0000-0000-0000-0000000000b0")?,
        plakous_model::BranchId::from_str("00000000-0000-0000-0000-0000000000c0")?,
    );
    let id = sessions.start(
        plakous_tools::JoinedProcessStartRequest::new(
            plakous_tools::JoinedProcessCommand::shell(
                plakous_tools::JoinedProcessShellProfile::new("/bin/sh", ["-c".to_owned()])?,
                "read go; seq 1 300000; read done",
            )?,
            ".",
            Vec::new(),
            true,
            plakous_tools::JoinedProcessTerminalMode::Pipes,
        )?,
        plakous_tools::ExecutionOperationControl::new(CancellationToken::new()),
    )?;
    plakous_tools::internal::promote(&sessions, id)?;
    let path = sessions
        .job_output_file(plakous_tools::JoinedProcessSessions::job_number(id))
        .ok_or("promotion issued no output file")?
        .path;
    sessions.write_stdin(
        id,
        b"go\n",
        plakous_tools::JoinedProcessInputDisposition::KeepOpen,
    )?;
    // About 2 MB: several read slices.
    let expected = (1..=300_000_u64)
        .map(|line| line.to_string().len() as u64 + 1)
        .sum::<u64>();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while fs::metadata(&path)?.len() < expected {
        assert!(std::time::Instant::now() < deadline, "spool incomplete");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let deep = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x71)?,
        "read_file",
        &serde_json::json!({"path": path, "offset": 250_000, "limit": 3}).to_string(),
    )
    .await?;
    let page: serde_json::Value = serde_json::from_str(text(&deep)?)?;
    assert_eq!(page["content"], "250000\n250001\n250002\n");
    assert_eq!(page["next_offset"], 250_003);
    let last = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x72)?,
        "read_file",
        &serde_json::json!({"path": path, "offset": 300_000}).to_string(),
    )
    .await?;
    let page: serde_json::Value = serde_json::from_str(text(&last)?)?;
    assert_eq!(page["content"], "300000\n");
    assert_eq!(page["eof"], true);
    let past = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x73)?,
        "read_file",
        &serde_json::json!({"path": path, "offset": 300_001}).to_string(),
    )
    .await?;
    assert_eq!(text(&past)?, "read offset is past the end of the file");
    sessions.write_stdin(
        id,
        b"done\n",
        plakous_tools::JoinedProcessInputDisposition::Close,
    )?;
    sessions.join_all()?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn read_file_does_not_report_a_valid_utf8_overlong_job_output_line_lossy() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x90)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let (sessions, _, path) = issued_job_output(&environment, state.path()).await?;
    // One valid UTF-8 line of 300 000 bytes (two-byte characters), longer
    // than a page, appended to the same issued inode.
    let mut file = fs::OpenOptions::new().append(true).open(&path)?;
    std::io::Write::write_all(
        &mut file,
        format!("{}\n", "\u{e9}".repeat(150_000)).as_bytes(),
    )?;
    drop(file);
    let read = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x91)?,
        "read_file",
        &serde_json::json!({"path": path, "offset": 3, "limit": 1}).to_string(),
    )
    .await?;
    let page: serde_json::Value = serde_json::from_str(text(&read)?)?;
    assert_eq!(page["clipped_lines"], 1);
    assert!(
        page.get("lossy").is_none(),
        "a valid UTF-8 job output line was reported lossy: {}",
        page["lossy"]
    );
    sessions.join_all()?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn read_file_pages_by_line_offset_without_a_digest_object() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let contents = (1..=250)
        .map(|line| format!("line-{line}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(workspace.path().join("long.txt"), contents)?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x7a)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);

    let first = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x7a)?,
        "read_file",
        r#"{"path":"long.txt","limit":200}"#,
    )
    .await?;
    let page: serde_json::Value = serde_json::from_str(text(&first)?)?;
    assert_eq!(page["offset"], 1);
    assert_eq!(page["lines"], 200);
    assert_eq!(page["eof"], false);
    assert_eq!(page["next_offset"], 201);
    let second = execute(
        &registry,
        environment,
        ids(0x7b)?,
        "read_file",
        r#"{"path":"long.txt","offset":201}"#,
    )
    .await?;
    let page: serde_json::Value = serde_json::from_str(text(&second)?)?;
    assert_eq!(page["offset"], 201);
    assert_eq!(page["eof"], true);
    assert!(text(&second)?.contains("line-250"));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn read_and_search_stamp_workspace_generation_when_policy_is_live() -> TestResult {
    use std::sync::atomic::{AtomicU64, Ordering};

    let workspace = tempfile::tempdir()?;
    fs::write(workspace.path().join("stamp.txt"), "needle-line\n")?;
    let counter = Arc::new(AtomicU64::new(7));
    let policy =
        WorkspacePolicy::host(workspace.path(), ".")?.with_generation_stamp(Arc::clone(&counter));
    let environment = Arc::new(ExecutionEnvironment::host_with_policy(
        ExecutionEnvironmentScope::Run(ids(0x8a)?.run),
        policy,
        ExecutionEnvironmentConfig::default(),
    )?);
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let read = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x8a)?,
        "read_file",
        r#"{"path":"stamp.txt"}"#,
    )
    .await?;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(text(&read)?)?["workspace_generation"],
        7
    );
    counter.store(8, Ordering::SeqCst);
    let search = execute(
        &registry,
        environment,
        ids(0x8b)?,
        "search_repository",
        r#"{"query":"needle","path":"."}"#,
    )
    .await?;
    let search_header = text(&search)?
        .lines()
        .next()
        .ok_or("search header missing")?;
    assert!(
        search_header.starts_with("workspace_generation=8 "),
        "search header was {search_header}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn repository_search_covers_more_than_sixteen_mib() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let contents = vec![b'x'; 1024 * 1024];
    for index in 0..32 {
        fs::write(workspace.path().join(format!("{index:02}.txt")), &contents)?;
    }
    fs::write(workspace.path().join("zz.txt"), b"needle\n")?;
    fs::write(
        workspace.path().join("oversize.txt"),
        vec![b'x'; 1024 * 1024 + 1],
    )?;
    fs::write(workspace.path().join("binary.bin"), [0])?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x91)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    for use_regex in [false, true] {
        let result = execute(
            &registry,
            Arc::clone(&environment),
            ids(0x91)?,
            "search_repository",
            &format!(r#"{{"query":"needle","path":".","use_regex":{use_regex}}}"#),
        )
        .await?;
        assert!(text(&result)?.contains("zz.txt:1:"), "{}", text(&result)?);
        assert!(text(&result)?.contains("needle"));
        let first = text(&result)?
            .lines()
            .next()
            .ok_or("missing search header")?;
        assert!(first.contains("1 files over 1024 KiB"), "{first}");
        assert!(first.contains("1 binary or non-UTF-8 files"), "{first}");
        assert!(!text(&result)?.contains("stopped"));
        assert!(first.contains("incomplete=false"), "{first}");
        assert!(!first.contains("narrow"), "{first}");
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn repository_search_names_scan_cap_before_reaching_byte_ceiling() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let contents = vec![b'x'; 1024 * 1024 - 1];
    for index in 0..257 {
        fs::write(workspace.path().join(format!("{index:03}.txt")), &contents)?;
    }
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x91)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    for use_regex in [false, true] {
        let result = execute(
            &registry,
            Arc::clone(&environment),
            ids(0x91)?,
            "search_repository",
            &format!(r#"{{"query":"needle","path":".","use_regex":{use_regex}}}"#),
        )
        .await?;
        let first = text(&result)?.lines().next().ok_or("missing header")?;
        assert!(first.contains("scanned-byte bound: 256 MiB"), "{first}");
        assert!(first.contains("incomplete=true"), "{first}");
    }
    Ok(())
}

#[tokio::test]
async fn regex_search_clipping_reports_reserved_budget_once() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let mut contents = String::new();
    for _ in 0..25 {
        writeln!(contents, "needle{}", "x".repeat(4000))?;
    }
    fs::write(workspace.path().join("wide.txt"), contents)?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x91)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let result = execute(
        &registry,
        environment,
        ids(0x91)?,
        "search_repository",
        r#"{"query":"needle","path":".","use_regex":true}"#,
    )
    .await?;
    let first = text(&result)?.lines().next().ok_or("missing header")?;
    assert!(first.contains("output stopped at 63 KiB"), "{first}");
    assert!(first.contains("incomplete=false"), "{first}");
    assert_eq!(first.matches("narrow").count(), 1);
    Ok(())
}

#[tokio::test]
async fn projected_search_headers_name_the_exposed_parameters() -> TestResult {
    use plakous_compose::CodingPresentationExt as _;
    use plakous_tools::{CodingPresentation, ProviderModelProjectionIdentityV2};
    let workspace = tempfile::tempdir()?;
    for index in 0..10_001 {
        fs::write(
            workspace.path().join(format!("{index:04}.txt")),
            b"needle\n",
        )?;
    }
    let mut builder = ToolRegistryBuilder::default();
    plakous_tools::register_packs(
        &plakous_compose::standard_coding_packs_in_memory()?,
        &mut builder,
    )?;
    let registry = builder.freeze();
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x91)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let effective = serde_json::from_value(serde_json::to_value(Digest::sha256(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        &[b"search-header-test"],
    )?)?)?;
    // Claude's pack searches through the shell; Grok is the one vendor
    // vocabulary with its own search parameters left.
    let (kind, name, args, advice) = (
        CodingPresentation::Grok,
        "grep",
        r#"{"pattern":"needle","path":".","head_limit":1}"#,
        "narrow path or pattern",
    );
    let target = ProviderModelProjectionIdentityV2::new(
        plakous_model::IntegrationClass::Standard,
        "test.protocol",
        "test.route",
        "test.profile",
        "test-family",
        "test-model",
        "test-model",
        effective,
        plakous_tools::PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    let projection = kind.projection_map(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        target,
    )?;
    let request = ToolInvocationRequest::new(
        ids(0x91)?.run,
        ids(0x91)?.turn,
        ids(0x91)?.attempt,
        ids(0x91)?.call,
        ToolName::new(name)?,
        ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::default())?,
        Arc::<str>::from(args),
    );
    let (invocation, _) = projection.prepare(request)?.into_parts();
    let result = execute_invocation(
        &registry,
        Arc::clone(&environment),
        ids(0x91)?,
        invocation,
        CancellationToken::new(),
    )
    .await?;
    let first = text(&result)?.lines().next().ok_or("missing header")?;
    assert!(first.contains("incomplete=true"), "{first}");
    assert!(first.contains(advice), "{first}");
    assert!(
        !first.contains("filename_glob") && !first.contains("query"),
        "{first}"
    );
    assert_eq!(first.matches("narrow").count(), 1);
    Ok(())
}

#[tokio::test]
async fn repository_search_incompleteness_precedes_matches() -> TestResult {
    let workspace = tempfile::tempdir()?;
    for index in 0..260 {
        fs::write(
            workspace.path().join(format!("{index:03}.txt")),
            b"needle\n",
        )?;
    }
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x91)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    for mode in [
        r#""use_regex":true"#,
        r#""use_regex":false"#,
        r#""paths_only":true,"filename_glob":"*.txt""#,
    ] {
        let result = execute(
            &registry,
            Arc::clone(&environment),
            ids(0x91)?,
            "search_repository",
            &format!(r#"{{"query":"needle","path":".","head_limit":1,{mode}}}"#),
        )
        .await?;
        let first = text(&result)?.lines().next().ok_or("missing header")?;
        assert!(first.contains("incomplete=true"), "{first}");
        assert!(first.contains("bound"), "{first}");
        assert!(first.contains("narrow"), "{first}");
    }
    Ok(())
}

#[tokio::test]
async fn regex_search_honors_paths_only_counts_only_and_offset() -> TestResult {
    let workspace = tempfile::tempdir()?;
    fs::write(
        workspace.path().join("alpha.rs"),
        b"fn needle() {}\nfn other() {}\nfn needle_two() {}\n",
    )?;
    fs::write(workspace.path().join("beta.rs"), b"fn needle() {}\n")?;
    fs::write(workspace.path().join("skip.txt"), b"fn needle() {}\n")?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x91)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let files = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x91)?,
        "search_repository",
        r#"{"query":"needle","path":".","use_regex":true,"paths_only":true,"filename_glob":"*.rs"}"#,
    )
    .await?;
    let files_text = text(&files)?;
    assert!(files_text.contains("alpha.rs"));
    assert!(files_text.contains("beta.rs"));
    assert!(!files_text.contains("skip.txt"));
    assert!(!files_text.contains("fn needle"));
    let counts = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x92)?,
        "search_repository",
        r#"{"query":"needle","path":".","use_regex":true,"counts_only":true,"filename_glob":"*.rs"}"#,
    )
    .await?;
    let counts_text = text(&counts)?;
    assert!(counts_text.contains("alpha.rs:2"));
    assert!(counts_text.contains("beta.rs:1"));
    let offset = execute(
        &registry,
        environment,
        ids(0x93)?,
        "search_repository",
        r#"{"query":"needle","path":".","use_regex":true,"filename_glob":"*.rs","offset":1,"head_limit":1}"#,
    )
    .await?;
    let offset_text = text(&offset)?;
    assert_eq!(
        offset_text
            .lines()
            .filter(|line| line.contains("needle"))
            .count(),
        1
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn glob_paths_only_returns_stamped_paths_without_a_match_header() -> TestResult {
    let workspace = tempfile::tempdir()?;
    fs::create_dir(workspace.path().join("src"))?;
    fs::write(workspace.path().join("src/lib.rs"), b"fn alpha() {}\n")?;
    fs::write(workspace.path().join("src/skip.txt"), b"fn alpha() {}\n")?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x94)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let outcome = execute(
        &registry,
        environment,
        ids(0x94)?,
        "search_repository",
        r#"{"query":"**/*.rs","path":".","paths_only":true,"filename_glob":"**/*.rs"}"#,
    )
    .await?;
    let visible = text(&outcome)?;
    assert!(
        !visible.contains("matches="),
        "glob path list was {visible}"
    );
    assert!(!visible.contains(":1:1:"), "glob path list was {visible}");
    assert!(
        visible.contains("src/lib.rs"),
        "glob path list was {visible}"
    );
    assert!(
        !visible.contains("skip.txt"),
        "glob path list was {visible}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn portable_read_and_search_precancel_and_backend_failures_are_typed() -> TestResult {
    let workspace = tempfile::tempdir()?;
    fs::write(workspace.path().join("safe.txt"), b"safe")?;
    let host_registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x75)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir()?;
        fs::write(outside.path().join("secret"), b"secret")?;
        symlink(outside.path().join("secret"), workspace.path().join("link"))?;
        let linked = execute(
            &host_registry,
            Arc::clone(&environment),
            ids(0x75)?,
            "read_file",
            r#"{"path":"link"}"#,
        )
        .await;
        assert_eq!(
            runtime_error_kind(require_error(linked, "symlink escape succeeded")?.as_ref()),
            Some(ToolRuntimeErrorKind::WorkspacePolicy)
        );
    }
    for (call, tool, arguments) in [
        (0x76, "read_file", r#"{"path":"safe.txt"}"#),
        (0x77, "search_repository", r#"{"query":"safe","path":"."}"#),
    ] {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let cancelled = execute_with_cancellation(
            &host_registry,
            Arc::clone(&environment),
            ids(call)?,
            tool,
            arguments,
            cancellation,
        )
        .await;
        assert_eq!(
            runtime_error_kind(require_error(cancelled, "precancelled executor ran")?.as_ref()),
            Some(ToolRuntimeErrorKind::Cancelled)
        );
    }

    let backend = Arc::new(DeterministicExecutionBackend::new(
        [],
        DeterministicProcessPolicy::new(false, false, false),
        [],
    )?);
    let deterministic = Arc::new(ExecutionEnvironment::deterministic(
        ExecutionEnvironmentScope::Run(ids(0x78)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
        backend,
    )?);
    let deterministic_registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let backend_error = execute(
        &deterministic_registry,
        deterministic,
        ids(0x78)?,
        "search_repository",
        r#"{"query":"needle","path":"missing"}"#,
    )
    .await;
    assert_eq!(
        runtime_error_kind(require_error(backend_error, "backend error suppressed")?.as_ref()),
        Some(ToolRuntimeErrorKind::WorkspacePolicy)
    );
    Ok(())
}

#[tokio::test]
async fn portable_plan_negative_contracts_are_preserved() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let plan = Arc::new(InMemoryPlanState::new());
    let registry = registry(plan.clone())?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x79)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let oversized_args = serde_json::json!({
        "plan": [{"step": "é".repeat(MAX_STANDARD_PLAN_STEP_BYTES), "status": "pending"}]
    });
    let oversized = execute(
        &registry,
        environment,
        ids(0x7a)?,
        "update_plan",
        &oversized_args.to_string(),
    )
    .await;
    assert_eq!(
        runtime_error_kind(
            require_error(oversized, "oversized UTF-8 plan step accepted")?.as_ref()
        ),
        Some(ToolRuntimeErrorKind::InvalidArguments)
    );
    assert_eq!(plan.snapshot()?.revision(), 0);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn unterminated_patch_body_rejects_all_files_before_writes() -> TestResult {
    let workspace = tempfile::tempdir()?;
    fs::write(workspace.path().join("keep.txt"), b"old\n")?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x7a)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let arguments = serde_json::json!({"patch": "diff --git a/keep.txt b/keep.txt\n--- a/keep.txt\n+++ b/keep.txt\n@@ -1 +1 @@\n-old\n+new\ndiff --git a/new.txt b/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+token"});
    let outcome = execute(
        &registry,
        environment,
        ids(0x7a)?,
        "apply_patch",
        &arguments.to_string(),
    )
    .await?;
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    assert_eq!(fs::read(workspace.path().join("keep.txt"))?, b"old\n");
    assert!(!workspace.path().join("new.txt").exists());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn portable_patch_delete_and_unsupported_rename_copy_are_authoritative() -> TestResult {
    let workspace = tempfile::tempdir()?;
    fs::write(workspace.path().join("delete.txt"), b"remove me\n")?;
    fs::write(workspace.path().join("keep.txt"), b"keep me\n")?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x7b)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let delete = serde_json::json!({"patch": "diff --git a/delete.txt b/delete.txt\ndeleted file mode 100644\n--- a/delete.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-remove me\n"});
    let deleted = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x7b)?,
        "apply_patch",
        &delete.to_string(),
    )
    .await?;
    assert_eq!(deleted.kind(), ToolOutcomeKind::Success);
    assert!(!workspace.path().join("delete.txt").exists());

    for (call, metadata) in [
        (
            0x7c,
            "similarity index 100%\nrename from keep.txt\nrename to moved.txt\n",
        ),
        (
            0x7d,
            "similarity index 100%\ncopy from keep.txt\ncopy to copied.txt\n",
        ),
    ] {
        let unsupported = serde_json::json!({
            "patch": format!("diff --git a/keep.txt b/moved.txt\n{metadata}")
        });
        let rejected = execute(
            &registry,
            Arc::clone(&environment),
            ids(call)?,
            "apply_patch",
            &unsupported.to_string(),
        )
        .await?;
        assert_eq!(rejected.kind(), ToolOutcomeKind::Failure);
        assert_eq!(fs::read(workspace.path().join("keep.txt"))?, b"keep me\n");
    }
    assert!(!workspace.path().join("moved.txt").exists());
    assert!(!workspace.path().join("copied.txt").exists());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn read_content_preserves_exact_line_endings_and_metadata_looking_text() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x91)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    for contents in [
        "",
        "one\n",
        "one\r\ntwo\r\n",
        "one\ntwo",
        "offset=1 lines=1 eof=true next_offset=0\n",
        "\n\n",
        "λ\r\n尾",
    ] {
        fs::write(workspace.path().join("exact.txt"), contents)?;
        let outcome = execute(
            &registry,
            Arc::clone(&environment),
            ids(0x91)?,
            "read_file",
            r#"{"path":"exact.txt"}"#,
        )
        .await?;
        let read: plakous_tools::WorkspaceFileReadResult = serde_json::from_str(text(&outcome)?)?;
        assert_eq!(read.content.as_bytes(), contents.as_bytes());
        assert!(read.eof);
        assert_eq!(read.next_offset, None);
    }
    fs::write(workspace.path().join("exact.txt"), "one\r\ntwo\nthree")?;
    let outcome = execute(
        &registry,
        environment,
        ids(0x92)?,
        "read_file",
        r#"{"path":"exact.txt","offset":2,"limit":1}"#,
    )
    .await?;
    let read: plakous_tools::WorkspaceFileReadResult = serde_json::from_str(text(&outcome)?)?;
    assert_eq!(read.content, "two\n");
    assert_eq!(read.next_offset, Some(3));
    assert!(!read.eof);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn read_pages_past_the_slice_bound_clips_long_lines_and_caps_a_page() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x93)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    // A 3 MB log: line 30000 is far past one 768 KiB slice.
    let mut log = String::new();
    for line in 1..=40_000 {
        writeln!(log, "log line {line:05} {}", "-".repeat(60))?;
    }
    fs::write(workspace.path().join("big.log"), &log)?;
    let outcome = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x93)?,
        "read_file",
        r#"{"path":"big.log","offset":30000,"limit":2}"#,
    )
    .await?;
    let read: plakous_tools::WorkspaceFileReadResult = serde_json::from_str(text(&outcome)?)?;
    assert!(
        read.content.starts_with("log line 30000 "),
        "{}",
        read.content
    );
    assert_eq!(read.lines, 2);
    assert_eq!(read.next_offset, Some(30_002));
    assert!(!read.eof);

    // One line over 2000 characters is clipped with a marker and counted.
    fs::write(
        workspace.path().join("wide.txt"),
        "y".repeat(5_000) + "\nshort\n",
    )?;
    let outcome = execute(
        &registry,
        Arc::clone(&environment),
        ids(0x94)?,
        "read_file",
        r#"{"path":"wide.txt"}"#,
    )
    .await?;
    let read: plakous_tools::WorkspaceFileReadResult = serde_json::from_str(text(&outcome)?)?;
    assert_eq!(read.clipped_lines, 1);
    assert!(
        read.content
            .contains("[line clipped: 3000 more characters]\nshort\n")
    );
    assert!(read.eof);

    // A page stops at 256 KiB and says where to continue.
    let lines = (0..2_000)
        .map(|_| "z".repeat(1_900))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(workspace.path().join("heavy.txt"), lines)?;
    let outcome = execute(
        &registry,
        environment,
        ids(0x95)?,
        "read_file",
        r#"{"path":"heavy.txt","limit":2000}"#,
    )
    .await?;
    let read: plakous_tools::WorkspaceFileReadResult = serde_json::from_str(text(&outcome)?)?;
    assert!(read.content.len() <= 262_144);
    assert_eq!(read.next_offset, Some(1 + read.lines));
    assert!(!read.eof);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn nested_read_then_write_preserves_exact_file_bytes() -> TestResult {
    use plakous_compose::CodingPresentationExt as _;
    let mut builder = ToolRegistryBuilder::default();
    plakous_tools::register_packs(
        &plakous_compose::standard_coding_packs_in_memory()?,
        &mut builder,
    )?;
    let registry = builder.freeze();
    let effective = serde_json::from_value(serde_json::to_value(Digest::sha256(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        &[b"nested-exact-copy"],
    )?)?)?;
    let target = plakous_tools::ProviderModelProjectionIdentityV2::new(
        plakous_model::IntegrationClass::Standard,
        "xai.grok.responses",
        "xai.grok_subscription",
        "xai.grok.product-javascript.v1",
        "responses-family",
        "grok-test",
        "grok-test",
        effective,
        1,
    )?;
    let projection = plakous_tools::CodingPresentation::Grok.compile_nested_projection(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        target,
        &[ToolName::new("read_file")?, ToolName::new("write")?],
    )?;
    let workspace = tempfile::tempdir()?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x94)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    for content in [
        "",
        "one\n",
        "one\r\ntwo\r\n",
        "one\ntwo",
        "offset=1 lines=1 eof=true next_offset=0\n",
        "λ\r\n尾",
    ] {
        fs::write(workspace.path().join("seed.txt"), content)?;
        let call = ids(0x94)?;
        let make_request = |name: &str, arguments: String| -> TestResult<_> {
            Ok(ToolInvocationRequest::new(
                call.run,
                call.turn,
                call.attempt,
                call.call,
                ToolName::new(name)?,
                ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::default())?,
                arguments,
            ))
        };
        let (invocation, receipt) = projection
            .prepare(make_request(
                "read_file",
                r#"{"target_file":"seed.txt"}"#.to_owned(),
            )?)?
            .into_parts();
        let outcome = execute_invocation(
            &registry,
            Arc::clone(&environment),
            call,
            invocation,
            CancellationToken::new(),
        )
        .await?;
        let raised = projection.raise(&receipt.ok_or("missing read receipt")?, outcome)?;
        let read: plakous_tools::WorkspaceFileReadResult = serde_json::from_str(text(&raised)?)?;
        let arguments =
            serde_json::json!({"file_path":"copy.txt", "content":read.content}).to_string();
        let (invocation, receipt) = projection
            .prepare(make_request("write", arguments)?)?
            .into_parts();
        let outcome = execute_invocation(
            &registry,
            Arc::clone(&environment),
            call,
            invocation,
            CancellationToken::new(),
        )
        .await?;
        let raised = projection.raise(&receipt.ok_or("missing write receipt")?, outcome)?;
        assert_eq!(raised.kind(), ToolOutcomeKind::Success);
        assert_eq!(
            fs::read(workspace.path().join("copy.txt"))?,
            content.as_bytes()
        );
    }
    Ok(())
}

/// A model names the workspace root three ways. All three list it.
///
/// Grok Build documents `.` for `list_dir`, some models send the empty string,
/// and provider-native vocabularies conventionally send an absolute path.
#[cfg(unix)]
#[tokio::test]
async fn search_and_read_accept_every_spelling_of_the_workspace_root() -> TestResult {
    let workspace = tempfile::tempdir()?;
    fs::write(workspace.path().join("inside.txt"), b"needle\n")?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x87)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let absolute = workspace.path().canonicalize()?;
    let spellings = [
        serde_json::Value::from("."),
        serde_json::Value::from(""),
        serde_json::Value::from(absolute.to_string_lossy().into_owned()),
    ];
    for (index, path) in spellings.iter().enumerate() {
        let call = 0x88_u8.saturating_add(u8::try_from(index)?);
        let listed = execute(
            &registry,
            Arc::clone(&environment),
            ids(call)?,
            "search_repository",
            &serde_json::json!({"path": path, "query": "", "list_directory": true}).to_string(),
        )
        .await?;
        assert_eq!(listed.kind(), ToolOutcomeKind::Success);
        assert!(
            text(&listed)?.contains("inside.txt"),
            "list_dir missed the root for {path:?}"
        );
        let searched = execute(
            &registry,
            Arc::clone(&environment),
            ids(call.saturating_add(0x10))?,
            "search_repository",
            &serde_json::json!({"path": path, "query": "needle"}).to_string(),
        )
        .await?;
        assert_eq!(searched.kind(), ToolOutcomeKind::Success);
        assert!(
            text(&searched)?.contains("inside.txt:1:1:needle"),
            "search missed the root for {path:?}"
        );
    }
    Ok(())
}

/// A bound socket in the tree is surveyed past and reported, not fatal.
///
/// A concurrent test run binding sockets under the workspace once made every
/// second `grep` fail with a query abort; now the walk finishes and says how
/// many entries it could not traverse.
#[cfg(unix)]
#[tokio::test]
async fn search_reports_entries_it_could_not_traverse_instead_of_failing() -> TestResult {
    let workspace = tempfile::tempdir()?;
    fs::write(workspace.path().join("a.txt"), b"needle one\n")?;
    let _listener = std::os::unix::net::UnixListener::bind(workspace.path().join("socket"))?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x8f)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let searched = execute(
        &registry,
        environment,
        ids(0x8f)?,
        "search_repository",
        r#"{"query":"needle","path":"."}"#,
    )
    .await?;
    assert_eq!(searched.kind(), ToolOutcomeKind::Success);
    let rendered = text(&searched)?;
    assert!(rendered.contains("a.txt:1:1:needle one\n"), "{rendered}");
    assert!(rendered.contains("entries_skipped=1\n"), "{rendered}");
    Ok(())
}

/// Builds the Claude direct projection over the standard coding packs.
fn claude_standard_projection(
    registry: plakous_tools::FrozenToolRegistry,
) -> TestResult<plakous_tools::ToolProjectionMap> {
    use plakous_compose::CodingPresentationExt as _;
    let effective = serde_json::from_value(serde_json::to_value(Digest::sha256(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        &[b"claude-standard-projection"],
    )?)?)?;
    let target = plakous_tools::ProviderModelProjectionIdentityV2::new(
        plakous_model::IntegrationClass::Standard,
        "anthropic.messages",
        "anthropic.claude_subscription",
        "anthropic.claude.compat.test",
        "messages-family",
        "claude-test",
        "claude-test",
        effective,
        1,
    )?;
    Ok(plakous_tools::CodingPresentation::Claude.projection_map(
        plakous_tools::JobConfig::default(),
        registry,
        target,
    )?)
}

fn standard_coding_registry() -> TestResult<plakous_tools::FrozenToolRegistry> {
    let mut builder = ToolRegistryBuilder::default();
    plakous_tools::register_packs(
        &plakous_compose::standard_coding_packs_in_memory()?,
        &mut builder,
    )?;
    Ok(builder.freeze())
}

/// Claude `Read` reaches an issued job output file outside the workspace
/// through the job-output read exception.
#[cfg(unix)]
#[tokio::test]
async fn claude_read_reaches_an_issued_job_output_file() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let registry = standard_coding_registry()?;
    let projection = claude_standard_projection(registry.clone())?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0xb1)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let (sessions, _number, path) = issued_job_output(&environment, state.path()).await?;
    assert!(!path.starts_with(&workspace.path().display().to_string()));
    let request = ToolInvocationRequest::new(
        ids(0xb1)?.run,
        ids(0xb1)?.turn,
        ids(0xb1)?.attempt,
        ids(0xb1)?.call,
        ToolName::new("Read")?,
        ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::default())?,
        Arc::<str>::from(serde_json::json!({"file_path": path, "offset": 2}).to_string()),
    );
    let (invocation, _) = projection.prepare(request)?.into_parts();
    let read = execute_invocation(
        &registry,
        Arc::clone(&environment),
        ids(0xb1)?,
        invocation,
        CancellationToken::new(),
    )
    .await?;
    let page: serde_json::Value = serde_json::from_str(text(&read)?)?;
    assert_eq!(page["content"], "second\n");
    sessions.join_all()?;
    Ok(())
}

/// The Claude `Read` description must not say every path outside the
/// workspace is refused while issued job output files are admitted.
#[test]
fn claude_read_description_names_the_job_output_exception() -> TestResult {
    let projection = claude_standard_projection(standard_coding_registry()?)?;
    let read = projection
        .by_exposed(&ToolName::new("Read")?)
        .ok_or("Claude Read missing")?
        .description();
    assert!(
        !read.contains("other paths are refused") || read.contains("full output"),
        "{read}"
    );
    // The same words as the under-root refusal and the write-failure line.
    assert!(
        read.contains("issued by an earlier process")
            && read.contains("[output file incomplete: write failed after N bytes]"),
        "{read}"
    );
    Ok(())
}

/// The Claude `Bash` description says that a backgrounded command's output
/// file is named on a `full output:` line and is read with `Read`.
#[test]
fn claude_bash_description_names_the_output_file_and_read() -> TestResult {
    let projection = claude_standard_projection(standard_coding_registry()?)?;
    let bash = projection
        .by_exposed(&ToolName::new("Bash")?)
        .ok_or("Claude Bash missing")?
        .description();
    assert!(
        bash.contains("full output") && bash.contains("with Read"),
        "{bash}"
    );
    Ok(())
}

/// Canonical `edits` beside a `patch` (no `file_path`) are refused without
/// mutation, never dropped while the patch applies.
#[tokio::test]
async fn apply_patch_edits_beside_a_patch_are_refused_not_dropped() -> TestResult {
    let workspace = tempfile::tempdir()?;
    fs::write(workspace.path().join("seed.txt"), b"alpha\n")?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(0x9a)?.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let outcome = execute(
        &registry,
        environment,
        ids(0x9a)?,
        "apply_patch",
        &serde_json::json!({
            "patch": "diff --git a/new.txt b/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+created\n",
            "edits": [{"old_string": "alpha", "new_string": "omega"}]
        })
        .to_string(),
    )
    .await?;
    assert_eq!(
        outcome.kind(),
        ToolOutcomeKind::Failure,
        "edits were dropped: {}",
        text(&outcome)?
    );
    assert!(!workspace.path().join("new.txt").exists());
    assert_eq!(fs::read(workspace.path().join("seed.txt"))?, b"alpha\n");
    Ok(())
}
