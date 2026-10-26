use super::*;
use crate::local_composition::{
    LocalCompositionAttachment, LocalCompositionEngineArtifact, LocalCompositionProfile,
    ProductExtensionEvidence,
};
use plakous_compose::PythonHarnessSdkV1;
use plakous_model::{CallId, ToolCallBlock, ToolCallState, ToolCorrelation, ToolExecutionOwner};

struct SdkModel {
    scripted: Arc<plakous_test_support::ScriptedModel>,
    calls: AtomicUsize,
}
const CELLS: &[(&str, &str)] = &[
    (
        "files = await asyncio.gather(ctx.read('ref_seed'), ctx.read('seed'), ctx.read('seed'), ctx.read('seed'))\nawait fs.apply([fs.Write('result', files[0].text, expect='absent')])\nawait state.put('answer', {'n': 42})\nprint('saved', len(files))",
        "saved 4",
    ),
    (
        "print((await state.get('answer'))['n'])\nawait state.put('large', [None] * 100000)\ntry:\n    await state.get('large')\nexcept OverflowError:\n    print('quota refused')\ntry:\n    await state.put('bad key', 1)\nexcept ValueError:\n    print('invalid key')",
        "quota refused",
    ),
    (
        "await state.put('answer', {'n': 99})\nraise MemoryError('memory limit exceeded')",
        "MemoryError",
    ),
    (
        "await fs.apply([fs.Write('not-run', 'bad')])\nawait fs.apply(42)",
        "type errors; nothing ran",
    ),
    (
        "print((await state.get('answer'))['n'])\nprint((await proc.run(['/bin/echo', 'literal ; argv'])).output)\nprint((await proc.run(['/bin/cat'], stdin='stdin round trip')).output)\njob = await proc.start(['/bin/echo', 'fast job'])\nprint((await proc.join(job, timeout_s=2)).status)",
        "literal ; argv",
    ),
    ("print('x' * 20000)", "output bytes omitted"),
    (
        "print((await ctx.read(Ref('__REF__'), lines=(1, 1))).text.startswith('x'))",
        "True",
    ),
];

impl LanguageModel for SdkModel {
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
        let failure = || {
            ModelPreparationError::rejected(
                ModelPreparationErrorKind::InvalidConfiguration,
                "SDK driver fixture is invalid",
            )
        };
        let ordinal = plan
            .request()
            .input()
            .items()
            .iter()
            .flat_map(ConversationItem::blocks)
            .filter(|block| matches!(block, ContentBlock::ToolResult(_)))
            .count();
        let mut last_output = String::new();
        if ordinal > 0 {
            let latest = plan
                .request()
                .input()
                .items()
                .iter()
                .flat_map(ConversationItem::blocks)
                .filter_map(|block| {
                    if let ContentBlock::ToolResult(result) = block {
                        Some(result)
                    } else {
                        None
                    }
                })
                .last()
                .ok_or_else(failure)?;
            let text = latest
                .parts()
                .iter()
                .filter_map(|part| {
                    if let plakous_model::ToolResultPart::Text(text) = part {
                        Some(text.text())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                text.contains(CELLS[ordinal - 1].1),
                "cell {}: {text}",
                ordinal - 1
            );
            if ordinal == 2 {
                assert!(
                    text.contains("42") && text.contains("invalid key"),
                    "{text}"
                );
            }
            if ordinal == 5 {
                assert!(
                    text.contains("42")
                        && text.contains("stdin round trip")
                        && text.contains("exited"),
                    "failed cells must not publish state: {text}"
                );
            }
            last_output = text;
        }
        let id = Uuid::from_bytes(
            plan.digest().bytes()[..16]
                .try_into()
                .map_err(|_| failure())?,
        );
        let block = if ordinal < CELLS.len() {
            let code = if CELLS[ordinal].0.contains("__REF__") {
                let reference = last_output
                    .split_once("Ref('")
                    .and_then(|(_, rest)| rest.split_once("')"))
                    .map(|(id, _)| id)
                    .ok_or_else(failure)?;
                CELLS[ordinal].0.replace("__REF__", reference)
            } else {
                CELLS[ordinal].0.to_owned()
            };
            ContentBlock::ToolCall(
                ToolCallBlock::new(
                    CallId::try_from(id).map_err(|_| failure())?,
                    ToolName::new("run_py").map_err(|_| failure())?,
                    ToolExecutionOwner::Local,
                    ToolCallState::Requested,
                    ToolCorrelation::new(
                        Some(format!("sdk-{ordinal}")),
                        None,
                        plakous_model::ToolCallerLineage::Direct,
                    )
                    .map_err(|_| failure())?,
                    serde_json::json!({"code":code}).to_string(),
                )
                .map_err(|_| failure())?,
            )
        } else {
            ContentBlock::Text(TextBlock::new("completed", None).map_err(|_| failure())?)
        };
        let output = ConversationItem::new(
            EntryId::try_from(id).map_err(|_| failure())?,
            Actor::Assistant,
            vec![block],
            Provenance::ModelOutput,
        )
        .map_err(|_| failure())?;
        let inert = self.scripted.clone().prepare_call(plan.clone(), context)?;
        Ok(PreparedModelCall::new(inert.record().clone(), move |_| {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::AcqRel);
                let finish = if matches!(output.blocks()[0], ContentBlock::ToolCall(_)) {
                    FinishReason::ToolCalls
                } else {
                    FinishReason::Stop
                };
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

#[tokio::test]
async fn real_python_sdk_driver_values_edits_state_and_quotas() -> TestResult {
    let worker = std::env::var_os("PLAK_PY_HOST")
        .map(PathBuf::from)
        .ok_or("real Python worker prerequisite is missing")?;
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("seed"), "hello SDK\n")?;
    std::fs::write(directory.path().join("ref_seed"), "hello SDK\n")?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let process = Arc::new(plakous_tool_process::ProcessSessionTools::posix_sh()?);
    let mut registry = ToolRegistryBuilder::default();
    let packs = plakous_compose::standard_coding_packs(
        Arc::new(plakous_tool_plan::InMemoryPlanState::default()),
        Arc::clone(&process),
    );
    plakous_tools::register_packs(&packs, &mut registry)?;
    PythonHarnessSdkV1::register_into(&mut registry, process)?;
    let registry = registry.freeze();
    let facts = descriptor()?;
    let authority = RuntimeModelAuthority::new(
        &facts,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"sdk-driver"])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"sdk-driver-credential"])?,
        1,
        timestamp()?,
        None,
    )?;
    let model = Arc::new(SdkModel {
        scripted: Arc::new(
            plakous_test_support::ScriptedModel::new(facts.clone(), [])
                .with_runtime_authority(authority.clone())
                .with_context_observation(crate::test_model::context_observation()?),
        ),
        calls: AtomicUsize::new(0),
    });
    let definition = plakous_tools::standard_definition(
        "run_py",
        "SDK run",
        plakous_tools::object_schema(
            serde_json::json!({"code":{"type":"string","minLength":1,"maxLength":262_144}}),
            &["code"],
        ),
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )?;
    let configured = crate::test_model::configured_model_for_operation(
        model.clone(),
        facts.clone(),
        authority,
        [definition],
        LocalDurabilityDisposition::DurableRequired,
        OutputBoundDisposition::Enforced(NonZeroU32::new(128).ok_or("bound")?),
        plakous_model::ToolPresentationMode::Direct,
    )?;
    let target = plakous_tools::ProviderModelProjectionIdentityV2::new(
        facts.integration_class(),
        facts.protocol().as_str(),
        facts.route().as_str(),
        facts.profile().as_str(),
        facts.model_family().ok_or("family")?.as_str(),
        facts.requested_model().as_str(),
        facts.requested_model().as_str(),
        configured.configuration().effective_identity(),
        plakous_tools::PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    let exact = PythonHarnessSdkV1::compile(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        target,
    )?;
    let profile = LocalCompositionProfile::cell(&(&exact).into())?;
    let artifact = LocalCompositionEngineArtifact::monty_worker_v1(&worker)?;
    let evidence = ProductExtensionEvidence::cell(
        &profile,
        &artifact,
        Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[b"sdk-issuer"])?,
        Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[b"sdk-receipt"])?,
        configured.configuration(),
    )?;
    let plan_model: Arc<dyn LanguageModel> = model.clone();
    let spec = plakous_agent::HarnessProfileSpecBuilder::new()
        .id(plakous_agent::HarnessProfileId::new("test.sdk-driver")?)
        .tool_exposure_strategy(plakous_agent::ToolExposureStrategy::None)
        .cache_strategy(plakous_agent::CachePreferenceStrategy::PreferStablePrefix)
        .build()?;
    let plan = plakous_agent::compile_explicit_runtime_profile(
        &spec,
        plan_model.as_ref(),
        configured.configuration(),
        &registry,
        &workspace,
        Vec::new(),
    )?;
    let attachment = LocalCompositionAttachment::builder(profile)
        .engine_artifact(artifact)
        .product_extension_evidence(evidence)
        .build()?;
    let builder = crate::local_composition::attach_local_composition(
        AgentBuilder::new(configured, registry, workspace)
            .tool_policy(Arc::new(AlwaysAllowPolicy))
            .harness_plan(plan)
            .request_policy(AgentRequestPolicy::new(
                GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                ReasoningPolicy::default(),
            ))
            .identity_sources(
                Arc::new(FixedClock::new(timestamp()?)),
                Arc::new(SystemIdGenerator),
            ),
        Some(attachment),
    )?;
    let sessions = sqlite_test_sessions(&directory.path().join("session.sqlite"))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:real-sdk-driver")?,
            SessionRuntimeBinding::new(builder),
        ))
        .await?;
    let result = session
        .agent()
        .prompt("exercise the SDK")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("result"))?,
        "hello SDK\n"
    );
    assert!(!directory.path().join("not-run").exists());
    assert_eq!(model.calls.load(Ordering::Acquire), CELLS.len() + 1);
    session.release().await?;
    Ok(())
}
