//! A Code Cell through a subordinate handle, with usage set between the
//! children's mark and the store's ordinary threshold: the nested call is
//! the catchable backpressure the mark names, and nothing journals it.

use super::subordinate::{BETWEEN, MARK, durable_metered_store};
use super::*;
use crate::local_composition::{
    LocalCompositionAttachment, LocalCompositionEngineArtifact, LocalCompositionProfile,
    ProductExtensionEvidence,
};
use plakous_compose::ProductJavaScriptCompositionV1;
use plakous_model::{
    CallId, ToolCallBlock, ToolCallState, ToolCallerLineage, ToolCorrelation, ToolExecutionOwner,
    ToolResultPart,
};
use plakous_test_support::{
    ExpectedRequest, ModelScenario, ScenarioStart, ScenarioStep, ScriptedModel,
};
use std::path::{Path, PathBuf};

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

fn scenario(
    item: &ConversationItem,
    seed: &[u8],
    finish: FinishReason,
) -> TestResult<ModelScenario> {
    let digest = Digest::sha256(DigestDomain::Blob, 1, &[seed])?;
    let response = ModelResponse::new(
        vec![item.clone()],
        FinishDetails::new(finish, None),
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

fn scenarios() -> TestResult<Vec<ModelScenario>> {
    let source = "let note = 'admitted'; try { await tools.probe({}); } catch (error) { note = String(error); } text(note);";
    let call = ToolCallBlock::new(
        CallId::try_from(Uuid::from_u128(30))?,
        ToolName::new("exec_js")?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::new(
            Some("subordinate-cell".into()),
            None,
            ToolCallerLineage::Direct,
        )?,
        serde_json::to_string(&serde_json::json!({
            "source": source,
            "yield_time_ms": 10_000
        }))?,
    )?;
    let exec = ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(31))?,
        Actor::Assistant,
        vec![ContentBlock::ToolCall(call)],
        Provenance::ModelOutput,
    )?;
    let done = ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(32))?,
        Actor::Assistant,
        vec![ContentBlock::Text(TextBlock::new("settled", None)?)],
        Provenance::ModelOutput,
    )?;
    Ok(vec![
        scenario(&exec, b"subordinate-cell-exec", FinishReason::ToolCalls)?,
        scenario(&done, b"subordinate-cell-done", FinishReason::Stop)?,
    ])
}

fn registry(probe: Arc<Probe>) -> TestResult<FrozenToolRegistry> {
    let mut registry = ToolRegistryBuilder::default();
    plakous_tools::register_packs(
        &crate::tools::standard_coding_packs_in_memory()?,
        &mut registry,
    )?;
    registry.register(
        ToolRegistration::new(
            RegisteredToolId::new("subordinate-cell.probe")?,
            ToolExecutorId::new("subordinate-cell.probe")?,
            plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"subordinate-cell.probe"])?,
            ToolDefinition::new(
                ToolName::new("probe")?,
                "1.0.0",
                "Record one nested call.",
                ToolInputContract::structured(serde_json::json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }))?,
                ToolOutputContract::utf8(128)?,
                ToolConcurrency::Parallel,
                ToolEffect::ReadOnly,
            )?,
            probe,
        )
        .nested_eligible(),
    )?;
    ProductJavaScriptCompositionV1::register_markers_into(&mut registry)?;
    Ok(registry.freeze())
}

fn binding(
    root: &Path,
    worker: &Path,
    registry: FrozenToolRegistry,
) -> TestResult<SessionRuntimeBinding> {
    let facts = descriptor()?;
    let authority = RuntimeModelAuthority::new(
        &facts,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"subordinate-cell"])?,
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"subordinate-cell-credential"],
        )?,
        1,
        timestamp()?,
        None,
    )?;
    let model = Arc::new(
        ScriptedModel::new(facts.clone(), scenarios()?)
            .with_runtime_authority(authority.clone())
            .with_context_observation(crate::test_model::context_observation()?)
            .with_call_recovery(plakous_test_support::ScriptedRecoveryState::new()),
    );
    let runtime_model: Arc<dyn LanguageModel> = model.clone();
    let configured = crate::test_model::configured_model_for_operation(
        model,
        facts,
        authority,
        registry
            .definitions()
            .iter()
            .filter(|tool| ["exec_js", "wait_js"].contains(&tool.name().as_str()))
            .cloned(),
        LocalDurabilityDisposition::DurableRequired,
        OutputBoundDisposition::Enforced(NonZeroU32::new(128).ok_or("output bound")?),
        plakous_model::ToolPresentationMode::LocalCompositionOnly,
    )?;
    let target =
        crate::construction::provider_model_projection_identity(configured.configuration())?;
    let profile = LocalCompositionProfile::cell(
        &(&ProductJavaScriptCompositionV1::compile_for_kind(
            plakous_tools::JobConfig::default(),
            registry.clone(),
            target,
            plakous_tools::CodingPresentation::Pi,
            [ToolName::new("probe")?],
        )?)
            .into(),
    )?;
    let artifact = LocalCompositionEngineArtifact::quickjs_worker_v1(worker)?;
    let identity = Digest::sha256(
        DigestDomain::ExecutionEnvironment,
        1,
        &[b"subordinate-cell-evidence"],
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
    let plan = crate::construction::compile_local_composition_plan(
        plakous_agent::HarnessProfileId::new("test.subordinate-cell")?,
        &runtime_model,
        configured.configuration(),
        &registry,
        &workspace,
    )?;
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
    Ok(SessionRuntimeBinding::new(
        crate::local_composition::attach_local_composition(builder, Some(attachment))?,
    ))
}

fn history_text(agent: &Agent) -> String {
    let checkpoint = agent_checkpoint(agent);
    let mut text = String::new();
    for segment in checkpoint.manifest().canonical().history().segments() {
        for item in segment.items() {
            for block in item.blocks() {
                match block {
                    ContentBlock::Text(block) => {
                        text.push_str(block.text());
                        text.push('\n');
                    }
                    ContentBlock::ToolResult(result) => {
                        for part in result.parts() {
                            if let ToolResultPart::Text(block) = part {
                                text.push_str(block.text());
                                text.push('\n');
                            }
                        }
                    }
                    ContentBlock::ToolResultUpdate(update) => {
                        text.push_str(update.text().text());
                        text.push('\n');
                    }
                    _ => {}
                }
            }
        }
    }
    text
}

/// Usage sits between the mark and the store's ordinary threshold once the
/// cell is in flight. The nested call is catchable backpressure naming the
/// mark, no child edge is prepared, and the turn settles quiescent.
#[tokio::test]
async fn a_cell_nested_call_between_the_mark_and_the_ordinary_threshold_is_backpressure()
-> TestResult {
    let worker = std::env::var_os("PLAK_JS_HOST")
        .map(PathBuf::from)
        .ok_or("set PLAK_JS_HOST to the test build's plakous-js-host")?;
    let directory = tempfile::tempdir()?;
    let probe = Arc::new(Probe::default());
    let registry = registry(Arc::clone(&probe))?;
    let store = durable_metered_store(&directory.path().join("session.sqlite3"))?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::clone(&store) as Arc<dyn SessionStore>,
        RuntimeDurability::Required,
    ))?;
    let children = sessions.with_subordinate_admission();
    let CreateSessionOutcome::Created { session, .. } = children
        .create(CreateSessionRequest::new(
            SessionOperationId::new("subordinate:cell")?,
            binding(directory.path(), &worker, registry)?,
        ))
        .await?;
    store.raise_between.store(true, Ordering::SeqCst);
    let result = session
        .agent()
        .prompt("run the cell")
        .await?
        .result()
        .await?;
    let text = history_text(session.agent());
    assert!(
        text.contains("nested call refused under storage pressure"),
        "outcome {:?}\n{text}",
        result.outcome()
    );
    assert!(
        text.contains(&format!("subordinate ordinary writes stop at {MARK}")),
        "{text}"
    );
    assert_eq!(probe.0.load(Ordering::Acquire), 0, "the nested tool ran");
    assert_eq!(store.used.load(Ordering::SeqCst), BETWEEN);
    let checkpoint = agent_checkpoint(session.agent());
    assert!(
        !checkpoint
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .any(|record| matches!(record.kind(), DurableOperationKind::CompositionChild { .. })),
        "a child edge was prepared"
    );
    assert!(
        matches!(
            checkpoint.manifest().lifecycle(),
            RuntimeLifecycle::Quiescent
        ),
        "outcome {:?}: {:?}",
        result.outcome(),
        checkpoint.manifest().lifecycle()
    );
    assert!(
        !matches!(result.outcome(), TurnResultOutcome::RecoveryRequired(_)),
        "the cell did not settle: {:?}",
        result.outcome()
    );
    session.release().await?;
    drop(children);
    drop(sessions);
    store.shutdown().await?;
    Ok(())
}
