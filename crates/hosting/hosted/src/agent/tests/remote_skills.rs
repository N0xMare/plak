//! Remote skills discovery with the native Claude route and harness profile.

use super::*;
use crate::runner_reader::test_kit::{runner_serving, write_skill};
use plakous::model::CancellationToken;
use std::sync::atomic::{AtomicUsize, Ordering};

mod reads;

fn read_text(outcome: &ToolOutcome) -> &str {
    let Some(ToolResultPart::Text(text)) = outcome.parts().first() else {
        panic!("the read omitted text");
    };
    text.text()
}

struct CountSkillReads {
    inner: Arc<dyn plakous::tools::runner::RunnerLink>,
    reads: Arc<AtomicUsize>,
}

impl plakous::tools::runner::RunnerLink for CountSkillReads {
    fn call(
        &self,
        call: plakous::tools::runner::RunnerCall,
    ) -> plakous::tools::runner::RunnerCallFuture {
        if call.payload()["op"] == "read_skill_slice" {
            self.reads.fetch_add(1, Ordering::Relaxed);
        }
        self.inner.call(call)
    }

    fn open(
        &self,
        call: plakous::tools::runner::RunnerCall,
    ) -> plakous::tools::runner::RunnerOpenFuture {
        self.inner.open(call)
    }
}

async fn read_file(
    registry: &plakous::tools::FrozenToolRegistry,
    workspace: &Path,
    path: &Path,
) -> Result<ToolOutcome, Box<dyn std::error::Error>> {
    read_file_with_cancellation(registry, workspace, path, CancellationToken::new()).await
}

async fn read_file_with_cancellation(
    registry: &plakous::tools::FrozenToolRegistry,
    workspace: &Path,
    path: &Path,
    cancellation: CancellationToken,
) -> Result<ToolOutcome, Box<dyn std::error::Error>> {
    use plakous::model::{Clock, FixedClock, Timestamp, ToolCorrelation, ToolName};
    use plakous_tools::{
        EffectAuthorizationGrantSource, ProgressSink, ToolInvocationRequest, ToolPolicyRevision,
    };
    let run = uuid::Uuid::from_u128(1).try_into()?;
    let turn = uuid::Uuid::from_u128(2).try_into()?;
    let attempt = uuid::Uuid::from_u128(3).try_into()?;
    let call = uuid::Uuid::from_u128(4).try_into()?;
    let environment = Arc::new(
        plakous::tools::ExecutionEnvironment::host(
            plakous::tools::ExecutionEnvironmentScope::Run(run),
            workspace,
            ".",
            plakous::tools::ExecutionEnvironmentConfig::default(),
        )?
        .with_line_owner(plakous::tools::LineOwner::new(
            uuid::Uuid::from_u128(42).try_into()?,
            1,
            uuid::Uuid::from_u128(43).try_into()?,
            0,
        )),
    );
    let invocation = registry.prepare(ToolInvocationRequest::new(
        run,
        turn,
        attempt,
        call,
        ToolName::new("read_file")?,
        ToolCorrelation::new(None, None, ToolCallerLineage::default())?,
        Arc::<str>::from(serde_json::json!({"path": path}).to_string()),
    ))?;
    let binding = plakous_tools::internal::authorization_binding(
        &invocation,
        environment.descriptor().clone(),
        ToolPolicyRevision::new("remote-skill-test", 1)?,
    )?;
    let grant = plakous_tools::internal::authorization_grant(
        binding,
        EffectAuthorizationGrantSource::Policy,
    );
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(Timestamp::new(71, 0)?));
    let (progress, _receiver) = ProgressSink::channel(8, call, clock.clone())?;
    let context = plakous_tools::internal::tool_context(
        run,
        turn,
        attempt,
        cancellation,
        None,
        clock,
        environment,
        grant,
        progress,
    );
    Ok(plakous_tools::internal::executor(registry, &invocation)?
        .execute(invocation, context)
        .await?)
}

#[test]
fn a_native_claude_profile_lists_and_reads_runner_home_skills()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let root = std::fs::canonicalize(root.path())?;
    let workspace_root = root.join("workspace");
    std::fs::create_dir_all(&workspace_root)?;
    let login_home = root.join("login");
    write_skill(&workspace_root, "review-pr");
    write_skill(&login_home, "host-only");
    let runner_home = root.join("runner-login");
    write_skill(&runner_home, "runner-only");
    let user_root = runner_home.join(".agents/skills");
    std::os::unix::fs::symlink(
        login_home.join(".agents/skills/host-only"),
        user_root.join("escape"),
    )?;
    std::os::unix::fs::symlink(user_root.join("runner-only"), user_root.join("inside"))?;
    let runner_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let (services, client) = runner_serving(&runner_runtime, &workspace_root, Some(&runner_home));
    let reads = Arc::new(AtomicUsize::new(0));
    let backend = services.backend();
    let services = plakous::runner::RunnerServices::new(
        Arc::new(plakous::tools::runner::RunnerBackend::new(
            serde_json::to_value(backend.descriptor())?,
            Arc::new(CountSkillReads {
                inner: backend.link(),
                reads: reads.clone(),
            }),
        )?),
        runner_runtime.handle().clone(),
    );
    let executor = plakous::application::ApplicationExecutor::new()?;
    let workspace = Arc::new(stamped_workspace_policy(
        &workspace_root,
        Path::new(""),
        Some(&services),
    )?);
    let model_id = ModelId::new("claude-opus-5-5")?;
    let composition = CompositionSelection::FollowCatalog;
    let selected = selected_coding_provider_configuration(
        NativeProvider::Claude,
        model_id.clone(),
        ReasoningEffort::High,
        None,
        composition,
    )?;
    // The same standard Claude OAuth model used by the native projection
    // fixtures. Opening must never ask it to dispatch paid inference.
    let resolved = standard_claude_model(&selected)?;
    let setting = crate::skills::SkillsSetting::new(true, Some(login_home));
    let skills = crate::skills::NodeSkills::discover(
        &setting,
        crate::skills::SkillsMode::New,
        &workspace_root,
        plakous::prompts::skills::SkillsPresentation::Claude,
        Some(&services),
    )?;
    assert_eq!(skills.read_roots(), vec![user_root.clone()]);
    let extensions = ProductToolExtensions::empty().with_skills(&skills)?;
    let extras = extensions.names().cloned().collect::<Vec<_>>();
    let registry =
        coding_tool_registry_with_extensions("remote-native", composition, false, &extensions)?;
    let read = executor.block_on(read_file(
        &registry,
        &workspace_root,
        &user_root.join("runner-only/SKILL.md"),
    ))??;
    let text = read_text(&read);
    assert!(text.contains("runner-only"), "{text}");
    assert_eq!(
        reads.load(Ordering::Relaxed),
        1,
        "the read must reach the runner, not the same-spelled host file"
    );
    let forged = executor.block_on(services.backend().link().call(
        plakous::tools::runner::RunnerCall::unkeyed(
            plakous::tools::runner::RunnerVerb::ReadInstructions,
            serde_json::json!({"op": "read_skill_slice", "offset": 0,
                "path": plakous::tools::runner::hex(workspace_root.join(".agents/skills/review-pr/SKILL.md").as_os_str().as_encoded_bytes()),
                "root": plakous::tools::runner::hex(workspace_root.as_os_str().as_encoded_bytes())}),
        ),
    ))?.unwrap_err();
    assert!(
        matches!(forged, plakous::tools::runner::RunnerFailure::Backend(ref value)
        if value["message"].as_str().is_some_and(|text| text.contains("does not serve this user skill root")))
    );
    executor.block_on(read_file(
        &registry,
        &workspace_root,
        &user_root.join("inside/SKILL.md"),
    ))??;
    let error = executor
        .block_on(read_file(
            &registry,
            &workspace_root,
            &user_root.join("escape/SKILL.md"),
        ))?
        .unwrap_err();
    assert!(
        error.to_string().contains("outside the skills directory"),
        "{error}"
    );
    let large = user_root.join("runner-only/large.txt");
    let mut body = vec![b'x'; plakous_tools::MAX_STANDARD_TEXT_BYTES as usize];
    body.extend_from_slice(b"\nsecond remote slice\n");
    std::fs::write(&large, body)?;
    let before = reads.load(Ordering::Relaxed);
    let read = executor.block_on(read_file(&registry, &workspace_root, &large))??;
    assert!(read_text(&read).contains("second remote slice"));
    assert_eq!(reads.load(Ordering::Relaxed) - before, 2);
    let jobs = plakous::tools::JobConfig::default();
    let node = node_session(
        jobs,
        resolved,
        &registry,
        &extensions,
        &extras,
        SessionToolFence::Full,
        None,
    )?;
    assert!(
        node.surface
            .exposed_names()
            .iter()
            .any(|name| name.as_str() == "Skill")
    );
    let backend = services.backend().descriptor().clone();
    let profile = selected_harness_profile(
        NativeProvider::Claude,
        &model_id,
        ReasoningEffort::High,
        false,
        false,
        &extras,
        false,
        composition,
        &node.surface,
        plakous::prompts::identity::PromptWorkspace::new(
            workspace.root(),
            workspace.cwd(),
            backend.operating_system(),
        ),
    )?
    .ok_or("no native profile")?;
    let (profile, sources) = enroll_product_instructions(
        &profile,
        None,
        workspace.root(),
        workspace.cwd(),
        extensions.skills_listing(),
        Some(&services),
    )?;
    let plan = plakous::agent::compile_explicit_runtime_profile(
        &profile,
        node.configured_model.model().as_ref(),
        &node.configuration,
        &registry,
        workspace.as_ref(),
        &backend,
        sources,
    )?;
    let builder =
        plakous::agent::AgentBuilder::new(node.configured_model, registry.clone(), workspace)
            .projection_admission(Arc::new(crate::coding_routes::ProductProjectionAdmission))
            .job_config(jobs)
            .tool_policy(session_tool_policy(SessionToolFence::Full))
            .tool_projection(node.surface.projection().clone())
            .harness_plan(plan)
            .request_policy(plakous::agent::AgentRequestPolicy::new(
                plakous::agent::GenerationLimits::new(
                    node.configuration
                        .effective()
                        .behavior()
                        .output_bound
                        .bound(),
                    Vec::new(),
                )?,
                plakous::agent::ReasoningPolicy::Enabled {
                    effort: Some(ReasoningEffort::High),
                    requirement: CapabilityRequirement::Required,
                },
            ));
    let sessions = plakous::sessions::AgentSessions::new(
        plakous::sessions::SessionBackend::sqlite(root.join("runtime.sqlite3"))?,
    )?;
    executor.block_on(async {
        let created = sessions
            .create(plakous::sessions::CreateSessionRequest::new(
                plakous::sessions::SessionOperationId::new("remote-native-create")?,
                plakous::sessions::SessionRuntimeBinding::new(builder)
                    .with_runner_services(services.clone()),
            ))
            .await?;
        let plakous::sessions::CreateSessionOutcome::Created { session, .. } = created else {
            return Err("expected a new native session".into());
        };
        let frozen = crate::skills::frozen_skills(&sessions, session.locator()).await?;
        assert!(frozen.on);
        let listing = frozen.listing.ok_or("missing native skill listing")?;
        assert!(
            listing.contains("review-pr")
                && listing.contains("runner-only")
                && !listing.contains("host-only"),
            "{listing}"
        );
        session.release().await?;
        Ok::<_, Box<dyn std::error::Error>>(())
    })??;
    client.disconnect();
    let error = executor
        .block_on(read_file(
            &registry,
            &workspace_root,
            &user_root.join("runner-only/SKILL.md"),
        ))?
        .unwrap_err();
    assert!(error.to_string().contains("runner"), "{error}");
    Ok(())
}
