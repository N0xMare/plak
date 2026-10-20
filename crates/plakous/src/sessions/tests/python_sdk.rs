//! `py-harness-sdk` through the facade: a scripted model drives `run_py`
//! cells on a real Monty worker through the real driver, against the real
//! canonical tools and a required-durability store.

use super::*;
use crate::local_composition::{
    LocalCompositionAttachment, LocalCompositionEngineArtifact, LocalCompositionProfile,
    ProductExtensionEvidence,
};
use crate::monty_python::HarnessSdkProgram;
use plakous_compose::{PythonHarnessSdkAccessV1, PythonHarnessSdkDescriptorV1, PythonHarnessSdkV1};
use plakous_model::{
    CallId, CompiledProviderConfiguration, ToolCallBlock, ToolCallState, ToolCorrelation,
    ToolExecutionOwner,
};
use plakous_tools::CodingPresentation;
use std::path::PathBuf;

/// Model rounds of one session, indexed by the assistant items already in the
/// request, so a request prepared twice answers the same way: `Some(code)`
/// calls `run_py`, `None` (or no entry) ends the turn.
struct SdkModel {
    scripted: Arc<plakous_test_support::ScriptedModel>,
    steps: Vec<Option<String>>,
    /// Every text the model received, per round, in input order.
    seen: Mutex<BTreeMap<usize, Vec<String>>>,
}

impl SdkModel {
    /// The last tool-result text of round `round`.
    fn result_of(&self, round: usize) -> String {
        test_lock(&self.seen)
            .get(&round)
            .into_iter()
            .flatten()
            .rev()
            .find(|text| {
                text.starts_with("cell ")
                    || text.starts_with("type errors")
                    || text.starts_with("[untrusted")
            })
            .cloned()
            .unwrap_or_default()
    }

    fn rounds(&self) -> usize {
        test_lock(&self.seen).len()
    }
}

fn input_texts(plan: &ValidatedRequestPlan) -> Vec<String> {
    let mut texts = Vec::new();
    for block in plan
        .request()
        .input()
        .items()
        .iter()
        .flat_map(ConversationItem::blocks)
    {
        match block {
            ContentBlock::Text(text) => texts.push(text.text().to_owned()),
            ContentBlock::ToolResult(result) => {
                texts.extend(result.parts().iter().filter_map(|part| match part {
                    plakous_model::ToolResultPart::Text(text) => Some(text.text().to_owned()),
                    _ => None,
                }));
            }
            _ => {}
        }
    }
    texts
}

impl LanguageModel for SdkModel {
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
        let failure = || {
            ModelPreparationError::rejected(
                ModelPreparationErrorKind::InvalidConfiguration,
                "Python SDK driver fixture is invalid",
            )
        };
        let round = plan
            .request()
            .input()
            .items()
            .iter()
            .filter(|item| item.actor() == Actor::Assistant)
            .count();
        test_lock(&self.seen).insert(round, input_texts(&plan));
        let id = Uuid::from_bytes(
            plan.digest().bytes()[..16]
                .try_into()
                .map_err(|_| failure())?,
        );
        let block = match self.steps.get(round).cloned().flatten() {
            Some(code) => run_py(id, &code).map_err(|_| failure())?,
            None => ContentBlock::Text(TextBlock::new("done", None).map_err(|_| failure())?),
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

fn run_py(id: Uuid, code: &str) -> TestResult<ContentBlock> {
    Ok(ContentBlock::ToolCall(ToolCallBlock::new(
        CallId::try_from(id)?,
        ToolName::new("run_py")?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::new(
            Some(format!("sdk-{id}")),
            None,
            plakous_model::ToolCallerLineage::Direct,
        )?,
        serde_json::to_string(&json!({ "code": code }))?,
    )?))
}

fn monty_worker() -> Option<PathBuf> {
    std::env::var_os("PLAK_MONTY_WORKER")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()?
                .parent()?
                .parent()
                .map(|path| path.join("monty"))
        })
        .filter(|path| path.is_file())
}

/// The standard coding packs and the SDK-only capabilities over one process
/// service, with the `run_py` marker.
fn sdk_registry() -> TestResult<FrozenToolRegistry> {
    let process = Arc::new(plakous_tool_process::ProcessSessionTools::posix_sh()?);
    let mut registry = ToolRegistryBuilder::default();
    plakous_tools::register_packs(
        &crate::tools::standard_coding_packs(
            Arc::new(plakous_tool_plan::InMemoryPlanState::new()),
            Arc::clone(&process),
        ),
        &mut registry,
    )?;
    PythonHarnessSdkV1::register_tools_into(&mut registry, process)?;
    Ok(registry.freeze())
}

struct SdkAgent {
    model: Arc<SdkModel>,
    builder: AgentBuilder,
    descriptor: PythonHarnessSdkDescriptorV1,
    configuration: CompiledProviderConfiguration,
    registry: FrozenToolRegistry,
}

/// Everything but the attachment, so a test can forge the engine program.
fn sdk_agent(
    workspace: &Arc<WorkspacePolicy>,
    access: PythonHarnessSdkAccessV1,
    steps: impl IntoIterator<Item = Option<&'static str>>,
) -> TestResult<SdkAgent> {
    let registry = sdk_registry()?;
    let facts = descriptor()?;
    let authority = RuntimeModelAuthority::new(
        &facts,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"python-sdk"])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"python-sdk-credential"])?,
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
        steps: steps
            .into_iter()
            .map(|step| step.map(str::to_owned))
            .collect(),
        seen: Mutex::new(BTreeMap::new()),
    });
    let configured = crate::test_model::configured_model_for_operation(
        model.clone(),
        facts,
        authority,
        registry
            .definitions()
            .iter()
            .filter(|tool| tool.name().as_str() == plakous_tools::PYTHON_HARNESS_SDK_RUN_NAME_V1)
            .cloned(),
        LocalDurabilityDisposition::DurableRequired,
        OutputBoundDisposition::Enforced(NonZeroU32::new(128).ok_or("output bound")?),
        plakous_model::ToolPresentationMode::LocalCompositionOnly,
    )?;
    let configuration = configured.configuration().clone();
    let descriptor = PythonHarnessSdkV1::compile(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        crate::construction::provider_model_projection_identity(&configuration)?,
        CodingPresentation::Pi,
        access,
    )?;
    let plan_model: Arc<dyn LanguageModel> = model.clone();
    let plan = crate::construction::compile_local_composition_plan(
        plakous_agent::HarnessProfileId::new("test.python-sdk")?,
        &plan_model,
        &configuration,
        &registry,
        workspace,
    )?;
    let builder = AgentBuilder::new(configured, registry.clone(), Arc::clone(workspace))
        .harness_plan(plan)
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::default(),
        ))
        .identity_sources(
            Arc::new(FixedClock::new(timestamp()?)),
            Arc::new(SystemIdGenerator),
        );
    Ok(SdkAgent {
        model,
        builder,
        descriptor,
        configuration,
        registry,
    })
}

fn sdk_attachment(agent: &SdkAgent, worker: &Path) -> TestResult<LocalCompositionAttachment> {
    let profile = LocalCompositionProfile::python_harness_sdk_v1(&agent.descriptor)?;
    let artifact = LocalCompositionEngineArtifact::monty_pool_harness_sdk_v1(worker)?;
    let evidence = ProductExtensionEvidence::python_harness_sdk_v1(
        &profile,
        &artifact,
        Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[b"sdk-issuer"])?,
        Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[b"sdk-candidate"])?,
        &agent.configuration,
    )?;
    Ok(LocalCompositionAttachment::builder(profile)
        .engine_artifact(artifact)
        .product_extension_evidence(evidence)
        .coding_presentation_kind(CodingPresentation::Pi)
        .build()?)
}

async fn create(
    directory: &TempDir,
    builder: AgentBuilder,
    operation: &str,
) -> TestResult<OpenedSession> {
    let sessions = sqlite_test_sessions(&directory.path().join("sessions.sqlite3"))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new(operation)?,
            SessionRuntimeBinding::new(builder),
        ))
        .await?;
    Ok(session)
}

/// Runs one turn to completion and returns the canonical names of the
/// nested children it completed, in order.
async fn completed_turn(session: &OpenedSession, prompt: &str) -> TestResult<Vec<String>> {
    let mut turn = session.agent().prompt(prompt).await?;
    let mut events = turn.take_events()?;
    let mut nested = Vec::new();
    while let Ok(event) = events.recv().await {
        match event.kind() {
            plakous_agent::AgentEventKind::ToolCompleted { identity, .. }
                if identity.canonical_name().as_str() != "run_py" =>
            {
                nested.push(identity.canonical_name().as_str().to_owned());
            }
            plakous_agent::AgentEventKind::TurnCompleted { .. }
            | plakous_agent::AgentEventKind::TurnFailed { .. } => break,
            _ => {}
        }
    }
    let result = turn.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    Ok(nested)
}

/// The model sees `run_py` alone; the nested catalog is the canonical SDK
/// set, with `edit_files` exactly under the full ceiling.
#[test]
fn the_model_sees_only_run_py_under_both_access_ceilings() -> TestResult {
    let directory = TempDir::new()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let worker = directory.path().join("worker");
    std::fs::write(&worker, b"stand-in worker bytes")?;
    let worker = worker.canonicalize()?;
    for access in [
        PythonHarnessSdkAccessV1::Full,
        PythonHarnessSdkAccessV1::ReadExecute,
    ] {
        let agent = sdk_agent(&workspace, access, [])?;
        let outer = agent
            .descriptor
            .outer_projection()
            .entries()
            .iter()
            .map(|entry| entry.exposed_name().as_str().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(outer, ["run_py"]);
        let nested = agent
            .descriptor
            .nested_tools()
            .iter()
            .map(|tool| tool.canonical_name().as_str().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            nested.iter().any(|name| name == "edit_files"),
            access == PythonHarnessSdkAccessV1::Full
        );
        assert!(nested.iter().all(|name| {
            agent
                .registry
                .identity(&ToolName::new(name.clone()).expect("nested name"))
                .is_some_and(|identity| {
                    identity.nested_eligibility()
                        == plakous_tools::NestedToolEligibility::NestedEligible
                })
        }));
        let attachment = sdk_attachment(&agent, &worker)?;
        crate::local_composition::attach_local_composition(agent.builder, Some(attachment))?;
    }
    Ok(())
}

/// Gate A: named state survives a cell while globals do not, a typed edit
/// applies at the read revision, and an argv process runs, all through the
/// real driver and the real canonical tools.
#[tokio::test]
async fn gate_a_state_survives_cells_and_edits_and_processes_apply() -> TestResult {
    let Some(worker) = monty_worker() else {
        eprintln!("skipping real Monty SDK driver test: worker unavailable");
        return Ok(());
    };
    let directory = TempDir::new()?;
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root)?;
    std::fs::write(root.join("notes.txt"), "alpha\n")?;
    let workspace = Arc::new(WorkspacePolicy::host(&root, ".")?);
    let agent = sdk_agent(
        &workspace,
        PythonHarnessSdkAccessV1::Full,
        [
            Some(
                "f = await ctx.read(\"notes.txt\")\n\
                 secret = f.text\n\
                 await state.put(\"k\", {\"text\": f.text, \"revision\": f.revision, \"n\": [1, 2.5, None, True, \"s\"], \"m\": {\"x\": \"y\"}})\n\
                 print(secret.strip())\n\
                 len(f.text)",
            ),
            None,
            Some(
                "try:\n    print(secret)  # type: ignore\nexcept NameError:\n    print(\"globals are fresh\")\n\
                 v = await state.get(\"k\")\n\
                 print(v[\"n\"], v[\"m\"])\n\
                 changes = await fs.apply(fs.Replace(\"notes.txt\", \"alpha\", \"beta\", expect=v[\"revision\"]))\n\
                 print([(c.path, c.action) for c in changes])\n\
                 r = await proc.run([\"printf\", \"x\"])\n\
                 print(r.status, r.exit_code, r.output)\n\
                 v == {\"text\": \"alpha\\n\", \"revision\": v[\"revision\"], \"n\": [1, 2.5, None, True, \"s\"], \"m\": {\"x\": \"y\"}}",
            ),
            None,
        ],
    )?;
    let model = Arc::clone(&agent.model);
    let attachment = sdk_attachment(&agent, &worker)?;
    let builder =
        crate::local_composition::attach_local_composition(agent.builder, Some(attachment))?;
    let session = create(&directory, builder, "create:python-sdk-gate-a").await?;

    let first = completed_turn(&session, "read and remember").await?;
    assert_eq!(model.result_of(1), "cell completed\nalpha\n6");
    assert_eq!(first, ["read_file"]);

    let second = completed_turn(&session, "edit and run").await?;
    assert_eq!(
        model.result_of(3),
        "cell completed\nglobals are fresh\n[1, 2.5, None, True, 's'] {'x': 'y'}\n\
         [('notes.txt', 'update')]\nexited 0 x\nTrue"
    );
    assert_eq!(std::fs::read_to_string(root.join("notes.txt"))?, "beta\n");
    assert_eq!(second.first().map(String::as_str), Some("edit_files"));
    assert!(second.iter().any(|name| name == "process_session_exec"));
    assert_eq!(model.rounds(), 4);
    session.release().await?;
    Ok(())
}

/// A cell that fails its type check runs nothing: no nested child, no
/// named-state write and no workspace effect.
#[tokio::test]
async fn a_type_error_runs_nothing() -> TestResult {
    let Some(worker) = monty_worker() else {
        eprintln!("skipping real Monty SDK driver test: worker unavailable");
        return Ok(());
    };
    let directory = TempDir::new()?;
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root)?;
    std::fs::write(root.join("notes.txt"), "alpha\n")?;
    let workspace = Arc::new(WorkspacePolicy::host(&root, ".")?);
    let agent = sdk_agent(
        &workspace,
        PythonHarnessSdkAccessV1::Full,
        [
            Some(
                "await state.put(\"ran\", True)\n\
                 await fs.apply(fs.Replace(\"notes.txt\", \"alpha\", \"beta\", count=\"many\"))",
            ),
            None,
            Some("await state.get(\"ran\", \"absent\")"),
            None,
        ],
    )?;
    let model = Arc::clone(&agent.model);
    let attachment = sdk_attachment(&agent, &worker)?;
    let builder =
        crate::local_composition::attach_local_composition(agent.builder, Some(attachment))?;
    let session = create(&directory, builder, "create:python-sdk-type-error").await?;

    let refused = completed_turn(&session, "edit wrongly").await?;
    assert!(
        model.result_of(1).starts_with("type errors; nothing ran\n"),
        "{}",
        model.result_of(1)
    );
    assert!(refused.is_empty(), "{refused:?}");
    assert_eq!(std::fs::read_to_string(root.join("notes.txt"))?, "alpha\n");
    completed_turn(&session, "check").await?;
    assert_eq!(model.result_of(3), "cell completed\n'absent'");
    session.release().await?;
    Ok(())
}

/// Under `read-execute` the stubs, the bootstrap and the nested catalog omit
/// `fs`. A forged engine program that still issues `edit_files` is refused by
/// the driver's nested admission, and nothing is written.
#[tokio::test]
async fn read_execute_admission_refuses_a_forged_edit() -> TestResult {
    let Some(worker) = monty_worker() else {
        eprintln!("skipping real Monty SDK driver test: worker unavailable");
        return Ok(());
    };
    let directory = TempDir::new()?;
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root)?;
    let workspace = Arc::new(WorkspacePolicy::host(&root, ".")?);
    let agent = sdk_agent(
        &workspace,
        PythonHarnessSdkAccessV1::ReadExecute,
        [
            Some(
                "try:\n    await fs.apply(fs.Write(\"forged.txt\", \"x\"))\n    print(\"written\")\n\
                 except RuntimeError as error:\n    print(\"refused:\", error)",
            ),
            None,
        ],
    )?;
    assert!(!agent.descriptor.stubs().contains("class fs"));
    let forged = PythonHarnessSdkV1::compile(
        plakous_tools::JobConfig::default(),
        agent.registry.clone(),
        crate::construction::provider_model_projection_identity(&agent.configuration)?,
        CodingPresentation::Pi,
        PythonHarnessSdkAccessV1::Full,
    )?;
    let program = HarnessSdkProgram {
        stubs: forged.stubs().to_owned(),
        bootstrap: forged.bootstrap().to_owned(),
        host_functions: forged
            .host_functions()
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
        limits: forged.limits(),
    };
    let model = Arc::clone(&agent.model);
    let attachment = sdk_attachment(&agent, &worker)?;
    let builder = crate::local_composition::python_sdk::attach_with_program(
        agent.builder,
        attachment,
        program,
    )?;
    let session = create(&directory, builder, "create:python-sdk-forged-edit").await?;

    let nested = completed_turn(&session, "write anyway").await?;
    let observed = model.result_of(1);
    assert!(
        observed.starts_with("cell completed\nrefused: ")
            && observed.contains("edit_files is not in this cell's catalog"),
        "{observed}"
    );
    assert!(nested.is_empty(), "{nested:?}");
    assert!(!root.join("forged.txt").exists());
    session.release().await?;
    Ok(())
}

/// A job a cell starts is an ordinary promoted process: its completion is a
/// process notice in the next turn's input.
#[tokio::test]
async fn a_started_job_reports_in_the_next_turn_input() -> TestResult {
    let Some(worker) = monty_worker() else {
        eprintln!("skipping real Monty SDK driver test: worker unavailable");
        return Ok(());
    };
    let directory = TempDir::new()?;
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root)?;
    let workspace = Arc::new(WorkspacePolicy::host(&root, ".")?);
    let agent = sdk_agent(
        &workspace,
        PythonHarnessSdkAccessV1::Full,
        [
            Some(
                "job = await proc.start(\"sleep 1; printf done > done.txt; printf finished\")\nprint(job.id)",
            ),
            None,
            None,
        ],
    )?;
    let model = Arc::clone(&agent.model);
    let attachment = sdk_attachment(&agent, &worker)?;
    let builder =
        crate::local_composition::attach_local_composition(agent.builder, Some(attachment))?;
    let session = create(&directory, builder, "create:python-sdk-job").await?;

    completed_turn(&session, "start a job").await?;
    assert!(
        model.result_of(1).starts_with("cell completed\n"),
        "{}",
        model.result_of(1)
    );
    // The job's own effect proves it ran to its end; the notice is the
    // driver's at the next admission.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !root.join("done.txt").exists() {
        assert!(Instant::now() < deadline, "the job never finished");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    completed_turn(&session, "anything new?").await?;
    let next = test_lock(&model.seen)
        .get(&2)
        .ok_or("the next turn reached the model")?
        .join("\n");
    assert!(next.contains("<process_notice"), "{next}");
    assert!(next.contains("finished"), "{next}");
    session.release().await?;
    Ok(())
}
