//! Real interpreter loss at nested-result boundaries, followed by exact SQLite reopen.

use super::*;
use crate::local_composition::{
    LocalCompositionAttachment, LocalCompositionEngineArtifact, LocalCompositionProfile,
    ProductExtensionEvidence,
};
use crate::sessions::feed::{
    FeedChange, FeedCursor, FeedFact, FeedPage, FeedPageRequest, FeedTerminalOutcome,
    FeedTerminalRequest,
};
use plakous_compose::{ProductJavaScriptCompositionV1, PythonPtcPresentationV1, PythonPtcV1};
use plakous_model::{
    CallId, HistorySegment, ToolCallBlock, ToolCallState, ToolCorrelation, ToolExecutionOwner,
};
use plakous_test_support::{
    ExpectedRequest, ModelScenario, ScenarioStart, ScenarioStep, ScriptedModel,
};
use std::os::unix::fs::PermissionsExt as _;
mod ledger_retirement;

#[derive(Default)]
struct Probe(AtomicUsize);

impl plakous_tools::ToolExecutor for Probe {
    fn execute(
        self: Arc<Self>,
        _invocation: plakous_tools::ToolInvocation,
        _context: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        self.0.fetch_add(1, Ordering::AcqRel);
        Box::pin(async { ToolOutcome::success_text("nested result") })
    }
}

#[derive(Clone, Copy, Debug)]
enum Engine {
    JavaScript,
    Python,
}

impl Engine {
    const fn override_variable(self) -> &'static str {
        match self {
            Self::JavaScript => "PLAK_JS_HOST",
            Self::Python => "PLAK_PY_HOST",
        }
    }

    const fn binary(self) -> &'static str {
        match self {
            Self::JavaScript => "plakous-js-host",
            Self::Python => "plakous-py-host",
        }
    }

    const fn markers(self) -> [&'static str; 2] {
        match self {
            Self::JavaScript => ["exec_js", "wait_js"],
            Self::Python => ["exec_py", "wait_py"],
        }
    }

    fn arguments(self) -> serde_json::Value {
        match self {
            Self::JavaScript => {
                serde_json::json!({"source": "text('source started'); text(await tools.probe({}));", "yield_time_ms": 10_000})
            }
            Self::Python => {
                serde_json::json!({"code": "print('source started')\nprint(await probe({}))", "yield_time_ms": 10_000})
            }
        }
    }
}

fn scenario(engine: Engine) -> TestResult<ModelScenario> {
    let call = ToolCallBlock::new(
        CallId::try_from(Uuid::from_u128(10))?,
        ToolName::new(engine.markers()[0])?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::new(
            Some("real-cell-crash".into()),
            None,
            plakous_model::ToolCallerLineage::Direct,
        )?,
        serde_json::to_string(&engine.arguments())?,
    )?;
    let item = ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(11))?,
        Actor::Assistant,
        vec![ContentBlock::ToolCall(call)],
        Provenance::ModelOutput,
    )?;
    let digest = Digest::sha256(DigestDomain::Blob, 1, &[b"unbound-fixture-plan"])?;
    let response = ModelResponse::new(
        vec![item.clone()],
        FinishDetails::new(FinishReason::ToolCalls, None),
        None,
        None,
        digest,
    );
    Ok(ModelScenario::new(
        ExpectedRequest::Any,
        ScenarioStart::Immediate,
        vec![
            ScenarioStep::Emit(ModelEvent::ResponseStarted(ResponseStarted::new(
                None, digest,
            ))),
            ScenarioStep::Emit(ModelEvent::BlockStarted(BlockStarted::new(
                item.id(),
                OutputLocation::new(0, 0),
                BlockStart::Complete(item.blocks()[0].clone()),
            ))),
            ScenarioStep::Emit(ModelEvent::ResponseCompleted(response)),
        ],
    ))
}

fn registry(
    engine: Engine,
    probe: Arc<dyn plakous_tools::ToolExecutor>,
) -> TestResult<FrozenToolRegistry> {
    let mut registry = ToolRegistryBuilder::default();
    plakous_tools::register_packs(
        &crate::tools::standard_coding_packs_in_memory()?,
        &mut registry,
    )?;
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("real-cell-crash.probe")?, ToolExecutorId::new("real-cell-crash.probe")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"real-cell-crash.probe"] )?,
        ToolDefinition::new(ToolName::new("probe")?, "1.0.0", "Record one nested call.",
            ToolInputContract::structured(serde_json::json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{},"required":[],"additionalProperties":false}))?,
            ToolOutputContract::utf8(128)?, ToolConcurrency::Parallel, ToolEffect::ReadOnly)?, probe,
    ).nested_eligible())?;
    match engine {
        Engine::JavaScript => ProductJavaScriptCompositionV1::register_markers_into(&mut registry)?,
        Engine::Python => PythonPtcV1::register_markers_into(&mut registry)?,
    }
    Ok(registry.freeze())
}

fn profile(
    engine: Engine,
    registry: FrozenToolRegistry,
    target: plakous_tools::ProviderModelProjectionIdentityV2,
) -> TestResult<LocalCompositionProfile> {
    Ok(match engine {
        Engine::JavaScript => LocalCompositionProfile::cell(
            &(&ProductJavaScriptCompositionV1::compile_for_kind(
                plakous_tools::JobConfig::default(),
                registry,
                target,
                plakous_tools::CodingPresentation::Pi,
                [ToolName::new("probe")?],
            )?)
                .into(),
        )?,
        Engine::Python => LocalCompositionProfile::cell(
            &(&PythonPtcV1::compile_for_kind(
                plakous_tools::JobConfig::default(),
                registry,
                target,
                PythonPtcPresentationV1::StructuredFunctions,
                plakous_tools::CodingPresentation::Pi,
                [ToolName::new("probe")?],
            )?)
                .into(),
        )?,
    })
}

fn binding(
    root: &Path,
    worker: &Path,
    engine: Engine,
    registry: FrozenToolRegistry,
    first: bool,
) -> TestResult<(SessionRuntimeBinding, Arc<ScriptedModel>)> {
    let scenarios = if first {
        vec![scenario(engine)?]
    } else {
        Vec::new()
    };
    binding_scenarios(
        root,
        worker,
        engine,
        registry,
        scenarios,
        plakous_agent::AgentConfig::default(),
        LocalDurabilityDisposition::DurableRequired,
    )
}

fn binding_scenarios(
    root: &Path,
    worker: &Path,
    engine: Engine,
    registry: FrozenToolRegistry,
    scenarios: Vec<ModelScenario>,
    config: plakous_agent::AgentConfig,
    durability: LocalDurabilityDisposition,
) -> TestResult<(SessionRuntimeBinding, Arc<ScriptedModel>)> {
    let facts = descriptor()?;
    let authority = RuntimeModelAuthority::new(
        &facts,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"real-cell-crash"])?,
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"real-cell-crash-credential"],
        )?,
        1,
        timestamp()?,
        None,
    )?;
    let model = Arc::new(
        ScriptedModel::new(facts.clone(), scenarios)
            .with_runtime_authority(authority.clone())
            .with_context_observation(crate::test_model::context_observation()?)
            .with_call_recovery(plakous_test_support::ScriptedRecoveryState::new()),
    );
    let runtime = binding_model(
        root,
        worker,
        engine,
        registry,
        config,
        durability,
        (model.clone(), authority),
    )?;
    Ok((runtime, model))
}

fn binding_model(
    root: &Path,
    worker: &Path,
    engine: Engine,
    registry: FrozenToolRegistry,
    config: plakous_agent::AgentConfig,
    durability: LocalDurabilityDisposition,
    model: (Arc<dyn LanguageModel>, RuntimeModelAuthority),
) -> TestResult<SessionRuntimeBinding> {
    let (model, authority) = model;
    let facts = descriptor()?;
    let configured = crate::test_model::configured_model_for_operation(
        model.clone(),
        facts,
        authority,
        registry
            .definitions()
            .iter()
            .filter(|tool| engine.markers().contains(&tool.name().as_str()))
            .cloned(),
        durability,
        OutputBoundDisposition::Enforced(NonZeroU32::new(128).ok_or("output bound")?),
        plakous_model::ToolPresentationMode::LocalCompositionOnly,
    )?;
    let target =
        crate::construction::provider_model_projection_identity(configured.configuration())?;
    let profile = profile(engine, registry.clone(), target)?;
    let artifact = match engine {
        Engine::JavaScript => LocalCompositionEngineArtifact::quickjs_worker_v1(worker)?,
        Engine::Python => LocalCompositionEngineArtifact::monty_worker_v1(worker)?,
    };
    let identity = Digest::sha256(
        DigestDomain::ExecutionEnvironment,
        1,
        &[b"real-cell-crash-evidence"],
    )?;
    let evidence = ProductExtensionEvidence::cell(
        &profile,
        &artifact,
        identity,
        identity,
        configured.configuration(),
    )?;
    let attachment = LocalCompositionAttachment::builder(profile)
        .engine_artifact(artifact)
        .coding_presentation_kind(plakous_tools::CodingPresentation::Pi)
        .product_extension_evidence(evidence)
        .build()?;
    let workspace = Arc::new(WorkspacePolicy::host(root, ".")?);
    let runtime_model: Arc<dyn LanguageModel> = model.clone();
    let plan = crate::construction::compile_local_composition_plan(
        plakous_agent::HarnessProfileId::new("test.real-cell-crash")?,
        &runtime_model,
        configured.configuration(),
        &registry,
        &workspace,
    )?;
    let builder = AgentBuilder::new(configured, registry, workspace)
        .config(config)
        .harness_plan(plan)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::default(),
        ))
        .identity_sources(
            Arc::new(FixedClock::new(timestamp()?)),
            Arc::new(SystemIdGenerator),
        );
    Ok(SessionRuntimeBinding::new(
        crate::local_composition::attach_local_composition(builder, Some(attachment))?,
    ))
}

fn wrapper(root: &Path, engine: Engine, cut: &str) -> TestResult<PathBuf> {
    let variable = engine.override_variable();
    let worker = std::env::var_os(variable)
        .map(PathBuf::from)
        .ok_or_else(|| format!("set {variable} to the test build's {} (just steps tests-durability resolves Cargo artifacts)", engine.binary()))?;
    let capability = Command::new(&worker)
        .arg("--test-cell-hooks")
        .output()
        .map_err(|error| format!("cannot run {variable}={}: {error}", worker.display()))?;
    if !capability.status.success() || capability.stdout != b"cell crash hooks enabled\n" {
        return Err(format!(
            "{variable}={} lacks debug test-hooks; build {} with --features test-hooks before the durability test",
            worker.display(), engine.binary()
        )
        .into());
    }
    let path = root.join("worker");
    for value in [root, worker.as_path()] {
        assert!(!value.to_string_lossy().contains('\''));
    }
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{root}/worker.pid'\nprintf 'launch\\n' >> '{root}/launches'\nexport PLAK_TEST_CELL_CUT='{cut}'\nexport PLAK_TEST_CELL_CUT_READY='{root}/ready'\nexec '{worker}'\n",
            root = root.display(),
            worker = worker.display()
        ),
    )?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    Ok(path.canonicalize()?)
}

async fn crash_case(engine: Engine, cut: &str) -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    let worker = wrapper(&root, engine, cut)?;
    let database = root.join("session.sqlite3");
    let probe = Arc::new(Probe::default());
    let registry = registry(engine, probe.clone())?;
    let sessions = AgentSessions::new(SessionBackend::sqlite(&database)?)?;
    let (runtime, model) = binding(&root, &worker, engine, registry.clone(), true)?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:real-cell-crash")?,
            runtime,
        ))
        .await?;
    let turn = session.agent().prompt("run the real worker once").await?;
    let turn_id = turn.id();
    tokio::time::timeout(Duration::from_secs(30), async {
        while !std::fs::read_to_string(root.join("ready")).is_ok_and(|marker| marker == cut) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .map_err(|_| format!("{engine:?} did not reach {cut}"))?;
    assert_eq!(probe.0.load(Ordering::Acquire), 1);
    let pid = std::fs::read_to_string(root.join("worker.pid"))?;
    assert!(
        Command::new("kill")
            .args(["-KILL", &pid])
            .status()?
            .success()
    );
    let result = tokio::time::timeout(Duration::from_secs(30), turn.result()).await??;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::RecoveryRequired(error) if error.kind() == AgentErrorKind::OutcomeUnknown),
        "{:?}",
        result.outcome()
    );
    let source = match session.release().await {
        Ok(locator) => locator,
        Err(error) => error.released_locator().ok_or(error)?,
    };
    assert_eq!(model.captured_plans().len(), 1);
    drop(sessions);
    verify_reopen(
        &root,
        &worker,
        engine,
        probe,
        registry,
        source,
        (turn_id, cut),
    )
    .await
}

async fn verify_reopen(
    root: &Path,
    worker: &Path,
    engine: Engine,
    probe: Arc<Probe>,
    registry: FrozenToolRegistry,
    source: SessionLocator,
    (turn_id, cut): (TurnId, &str),
) -> TestResult {
    let reopened = AgentSessions::new(SessionBackend::sqlite(root.join("session.sqlite3"))?)?;
    let (runtime, fresh_model) = binding(root, worker, engine, registry, false)?;
    let opened = reopened
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:real-cell-crash")?,
            source,
            runtime,
        ))
        .await?;
    let OpenSessionOutcome::Opened { session, .. } = opened else {
        return Err(format!("exact reopen did not settle the lost cell: {opened:?}").into());
    };
    let target = session.locator();
    persisted_terminal(&reopened, target, turn_id).await?;
    assert_reopened_history(session.agent(), cut)?;
    assert_eq!(
        fresh_model.captured_plans().len(),
        0,
        "no paid/source replay"
    );
    assert_eq!(probe.0.load(Ordering::Acquire), 1, "no nested tool replay");
    assert_eq!(
        std::fs::read_to_string(root.join("launches"))?,
        "launch\n",
        "no evaluator restart"
    );
    session.release().await?;
    Ok(())
}

fn assert_reopened_history(agent: &Agent, cut: &str) -> TestResult {
    let checkpoint = agent_checkpoint(agent);
    let blocks: Vec<_> = checkpoint
        .manifest()
        .canonical()
        .history()
        .segments()
        .iter()
        .flat_map(HistorySegment::items)
        .flat_map(ConversationItem::blocks)
        .collect();
    let outer = CallId::try_from(Uuid::from_u128(10))?;
    assert_eq!(
        checkpoint.manifest().ledger().operations().into_iter()
            .filter(|record| matches!(record.kind(), DurableOperationKind::CompositionEvaluation { call_id, .. } if *call_id == outer))
            .count(),
        1,
        "retain the exact owning exec operation"
    );
    assert!(!blocks.iter().any(|block| matches!(block,
        ContentBlock::ToolResult(result) if result.call_id() == outer && result.kind() == plakous_model::ToolResultKind::Success
    )), "worker loss must not fabricate a successful outer cell result");
    let children: Vec<_> = checkpoint
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter(|record| matches!(record.kind(), DurableOperationKind::CompositionChild { .. }))
        .collect();
    assert_eq!(children.len(), 1, "one nested delivery obligation");
    assert!(
        matches!(
            children[0].state(),
            DurableOperationState::Completed(_) | DurableOperationState::Finalized(_)
        ),
        "{cut}: execution completed before delivery: {:?}",
        children[0].state()
    );
    let DurableOperationKind::CompositionChild { call_id, .. } = children[0].kind() else {
        return Err("nested edge missing".into());
    };
    let effects: Vec<_> = checkpoint.manifest().ledger().operations().into_iter().filter(|record| matches!(record.kind(), DurableOperationKind::Effect { call_id: id, .. } if id == call_id)).collect();
    assert_eq!(effects.len(), 1, "retain the exact nested effect");
    assert!(
        matches!(
            effects[0].state(),
            DurableOperationState::Completed(_) | DurableOperationState::Finalized(_)
        ),
        "{cut}: {:?}",
        effects[0].state()
    );
    Ok(())
}

async fn persisted_terminal(
    sessions: &AgentSessions,
    locator: SessionLocator,
    turn_id: TurnId,
) -> TestResult {
    let mut cursor = FeedCursor::start(locator.session_id());
    let mut checkpoints = Vec::new();
    loop {
        let FeedPage::Records { records, .. } =
            sessions.feed_page(FeedPageRequest::after(cursor)).await?
        else {
            return Err("unexpected feed retention gap".into());
        };
        if records.is_empty() {
            break;
        }
        for record in &records {
            for fact in record.facts() {
                if matches!(fact, FeedFact::TurnTerminal { turn_id: id, .. } if *id == turn_id) {
                    let FeedChange::Advanced { checkpoint, .. } = record.header().change else {
                        return Err("terminal did not advance the branch".into());
                    };
                    checkpoints.push(checkpoint);
                }
            }
            cursor = cursor.advanced_past(record);
        }
    }
    assert_eq!(
        checkpoints.len(),
        1,
        "recovery must not publish another root terminal"
    );
    let FeedTerminalOutcome::Terminal(terminal) = sessions
        .feed_terminal(FeedTerminalRequest {
            session: locator.session_id(),
            branch: locator.branch_id(),
            checkpoint: checkpoints[0],
            turn_id,
        })
        .await?
    else {
        return Err("the worker-loss terminal was not retained".into());
    };
    let TurnResultOutcome::RecoveryRequired(error) = terminal.result().outcome() else {
        return Err("reopen relabeled uncertain interpreter execution as completion".into());
    };
    assert_eq!(error.kind(), AgentErrorKind::OutcomeUnknown);
    assert!(error.message().contains("recovery"), "{}", error.message());
    Ok(())
}

#[tokio::test]
async fn real_cell_worker_crash_recovery_boundaries() -> TestResult {
    for engine in [Engine::JavaScript, Engine::Python] {
        for cut in ["before_delivery", "after_accepted"] {
            Box::pin(crash_case(engine, cut)).await?;
        }
    }
    Ok(())
}
