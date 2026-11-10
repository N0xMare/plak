//! Remote skills discovery with the native Claude route and harness profile.

use super::*;
use crate::runner_reader::test_kit::{runner_serving, write_skill};

#[test]
fn a_native_claude_profile_opens_on_a_workspace_only_runner()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let root = std::fs::canonicalize(root.path())?;
    let workspace_root = root.join("workspace");
    std::fs::create_dir_all(&workspace_root)?;
    let login_home = root.join("login");
    write_skill(&workspace_root, "review-pr");
    write_skill(&login_home, "host-only");
    let runner_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let (services, client) = runner_serving(&runner_runtime, &workspace_root);
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
    assert!(skills.read_roots().is_empty());
    let extensions = ProductToolExtensions::empty().with_skills(&skills)?;
    let extras = extensions.names().cloned().collect::<Vec<_>>();
    let registry =
        coding_tool_registry_with_extensions("remote-native", composition, false, &extensions)?;
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
    let builder = plakous::agent::AgentBuilder::new(node.configured_model, registry, workspace)
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
            listing.contains("review-pr") && !listing.contains("host-only"),
            "{listing}"
        );
        session.release().await?;
        Ok::<_, Box<dyn std::error::Error>>(())
    })??;
    client.disconnect();
    Ok(())
}
