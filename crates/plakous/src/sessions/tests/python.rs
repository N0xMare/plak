use super::*;
use crate::local_composition::{
    LocalCompositionAttachment, LocalCompositionEngineArtifact, LocalCompositionProfile,
    ProductExtensionEvidence,
};
use plakous_compose::{PythonPtcPresentationV1, PythonPtcV1};
use plakous_model::{CallId, ToolCallBlock, ToolCallState, ToolCorrelation, ToolExecutionOwner};
use std::path::PathBuf;
use tokio::sync::Notify;

#[derive(Default)]
struct Probe {
    entered: Notify,
    release: Notify,
    calls: AtomicUsize,
}

impl plakous_tools::ToolExecutor for Probe {
    fn execute(
        self: Arc<Self>,
        _invocation: plakous_tools::ToolInvocation,
        _context: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        Box::pin(async move {
            if self.calls.fetch_add(1, Ordering::AcqRel) == 0 {
                self.entered.notify_one();
                self.release.notified().await;
                ToolOutcome::success_text("first")
            } else {
                ToolOutcome::success_text("second")
            }
        })
    }
}

struct PythonModel {
    scripted: Arc<plakous_test_support::ScriptedModel>,
    calls: AtomicUsize,
    waits: AtomicUsize,
}

impl LanguageModel for PythonModel {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        self.scripted
            .clone()
            .revalidate_runtime_authority(checked_at)
    }

    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        let ordinal = self.calls.fetch_add(1, Ordering::AcqRel);
        let failure = || {
            ModelPreparationError::rejected(
                ModelPreparationErrorKind::InvalidConfiguration,
                "Python driver fixture is invalid",
            )
        };
        let id = Uuid::from_bytes(
            plan.digest().bytes()[..16]
                .try_into()
                .map_err(|_| failure())?,
        );
        let latest = plan
            .request()
            .input()
            .items()
            .iter()
            .flat_map(ConversationItem::blocks)
            .filter_map(|block| match block {
                ContentBlock::ToolResult(result) => {
                    result.parts().iter().find_map(|part| match part {
                        plakous_model::ToolResultPart::Text(text) => Some(text.text()),
                        _ => None,
                    })
                }
                _ => None,
            })
            .last();
        let block = if ordinal == 0 {
            tool_call(
                id,
                "exec_py",
                &serde_json::json!({
                    "code": "print(await probe({}))\nprint(await probe({}))",
                    "yield_time_ms": 10_000,
                }),
            )
            .map_err(|_| failure())?
        } else if let Some(cell) = latest
            .and_then(|text| text.lines().next())
            .and_then(|status| status.strip_prefix("Python cell running with cell ID "))
        {
            self.waits.fetch_add(1, Ordering::AcqRel);
            tool_call(
                id,
                "wait_py",
                &serde_json::json!({"cell_id": cell, "yield_time_ms": 60_000}),
            )
            .map_err(|_| failure())?
        } else {
            assert!(
                latest.is_some_and(
                    |text| text.contains("Python cell completed") && text.contains("second")
                ),
                "wait_py must return the real interpreter result: {latest:?}"
            );
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

fn tool_call(id: Uuid, name: &str, arguments: &serde_json::Value) -> TestResult<ContentBlock> {
    Ok(ContentBlock::ToolCall(ToolCallBlock::new(
        CallId::try_from(id)?,
        ToolName::new(name)?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::new(
            Some(format!("py-{id}")),
            None,
            plakous_model::ToolCallerLineage::Direct,
        )?,
        serde_json::to_string(arguments)?,
    )?))
}

#[tokio::test]
async fn real_monty_driver_yields_steers_and_waits_for_the_result() -> TestResult {
    let worker = std::env::var_os("PLAK_MONTY_WORKER")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()?
                .parent()?
                .parent()
                .map(|p| p.join("monty"))
        });
    let Some(worker) = worker.filter(|path| path.is_file()) else {
        eprintln!("skipping real Monty driver test: worker unavailable");
        return Ok(());
    };
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let probe = Arc::new(Probe::default());
    let mut registry = ToolRegistryBuilder::default();
    plakous_tools::register_packs(
        &crate::tools::standard_coding_packs_in_memory()?,
        &mut registry,
    )?;
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("python-driver.probe")?, ToolExecutorId::new("python-driver.probe")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"python-driver"] )?,
        ToolDefinition::new(ToolName::new("probe")?, "1.0.0", "Read-only Python probe.",
            ToolInputContract::structured(serde_json::json!({"$schema": "https://json-schema.org/draft/2020-12/schema", "type": "object", "properties": {}, "required": [], "additionalProperties": false}))?,
            ToolOutputContract::model_content(128, 1024)?, ToolConcurrency::Parallel, ToolEffect::ReadOnly)?,
        probe.clone(),
    ).nested_eligible())?;
    PythonPtcV1::register_markers_into(&mut registry)?;
    let registry = registry.freeze();
    let facts = descriptor()?;
    let authority = RuntimeModelAuthority::new(
        &facts,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"python-driver"])?,
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"python-driver-credential"],
        )?,
        1,
        timestamp()?,
        None,
    )?;
    let model = Arc::new(PythonModel {
        scripted: Arc::new(
            plakous_test_support::ScriptedModel::new(facts.clone(), [])
                .with_runtime_authority(authority.clone())
                .with_context_observation(crate::test_model::context_observation()?),
        ),
        calls: AtomicUsize::new(0),
        waits: AtomicUsize::new(0),
    });
    let configured = crate::test_model::configured_model_for_operation(
        model.clone(),
        facts.clone(),
        authority,
        registry
            .definitions()
            .iter()
            .filter(|tool| ["exec_py", "wait_py"].contains(&tool.name().as_str()))
            .cloned(),
        LocalDurabilityDisposition::Ephemeral,
        OutputBoundDisposition::Enforced(NonZeroU32::new(128).ok_or("output bound")?),
        plakous_model::ToolPresentationMode::LocalCompositionOnly,
    )?;
    let target = plakous_tools::ProviderModelProjectionIdentityV2::new(
        facts.integration_class(),
        facts.protocol().as_str(),
        facts.route().as_str(),
        facts.profile().as_str(),
        facts.model_family().ok_or("model family")?.as_str(),
        facts.requested_model().as_str(),
        facts.requested_model().as_str(),
        configured.configuration().effective_identity(),
        plakous_tools::PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    let exact = PythonPtcV1::compile_for_kind(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        target,
        PythonPtcPresentationV1::StructuredFunctions,
        plakous_tools::CodingPresentation::Pi,
        [ToolName::new("probe")?],
    )?;
    let profile = LocalCompositionProfile::python_ptc_v1(&exact)?;
    let artifact = LocalCompositionEngineArtifact::monty_pool_v1(&worker)?;
    let evidence = ProductExtensionEvidence::python_ptc_v1(
        &profile,
        &artifact,
        Digest::sha256(
            DigestDomain::ExecutionEnvironment,
            1,
            &[b"python-driver-issuer"],
        )?,
        Digest::sha256(
            DigestDomain::ExecutionEnvironment,
            1,
            &[b"python-driver-candidate"],
        )?,
        configured.configuration(),
    )?;
    let plan_model: Arc<dyn LanguageModel> = model.clone();
    let plan = crate::construction::compile_local_composition_plan(
        plakous_agent::HarnessProfileId::new("test.python-driver")?,
        &plan_model,
        configured.configuration(),
        &registry,
        &workspace,
    )?;
    let attachment = LocalCompositionAttachment::builder(profile)
        .engine_artifact(artifact)
        .product_extension_evidence(evidence)
        .coding_presentation_kind(plakous_tools::CodingPresentation::Pi)
        .build()?;
    let builder = AgentBuilder::new(configured, registry, workspace)
        .harness_plan(plan)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::default(),
        ))
        .identity_sources(
            Arc::new(FixedClock::new(timestamp()?)),
            Arc::new(SystemIdGenerator),
        );
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(MemorySessionStore::new(MemoryStoreLimits::default())?),
        RuntimeDurability::Disabled,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:real-python-driver")?,
            SessionRuntimeBinding::new(crate::local_composition::attach_local_composition(
                builder,
                Some(attachment),
            )?),
        ))
        .await?;
    let turn = session.agent().prompt("open Python cell").await?;
    // The probe notifies when the cell is inside the first call. Waiting on
    // that event, not a 10s deadline: under gate load the worker was still
    // starting when the deadline elapsed. The yield budget below stays open
    // because it starts with the cell, which is this notification.
    probe.entered.notified().await;
    turn.control()
        .steer("continue with the existing Python cell")
        .await?;
    // The steer cannot yield while the first probe is in flight (§9.5
    // quiescence). Once its result is delivered, a yielding contract ends the
    // `exec_py` observation with the cell still running, well inside its
    // 10-second yield, and the model observes it with `wait_py`. A
    // non-yielding contract runs the cell to completion inside `exec_py`.
    probe.release.notify_one();
    let result = turn.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert!(
        model.waits.load(Ordering::Acquire) >= 2,
        "a steer yields the cell"
    );
    assert_eq!(probe.calls.load(Ordering::Acquire), 2, "no nested replay");
    session.release().await?;
    Ok(())
}
