//! Predecessor-only fixture producer, copied into its unit-test module by the recipe.

use super::*;

#[tokio::test]
#[ignore = "run only in the pinned predecessor checkout through the fixture recipe"]
async fn generate_previous_release_bindings() -> Result<(), Box<dyn std::error::Error>> {
    use crate::agent_tree_protocol::{SpawnAgentRequest, Tier, WriterMode};
    use plakous::sessions::{
        AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend,
        SessionOperationId, SessionRuntimeBinding,
    };
    let output = PathBuf::from(std::env::var("PLAK_BINDING_FIXTURE_OUTPUT")?);
    std::fs::create_dir_all(&output)?;
    let database = output.join("previous-runtime.sqlite3");
    assert!(!database.exists(), "never overwrite a predecessor fixture");
    let sessions = AgentSessions::new(SessionBackend::sqlite(&database)?)?;
    let workspace = Arc::new(stamped_workspace_policy(
        Path::new("/"),
        Path::new(""),
        None,
    )?);
    let mut cases = Vec::new();
    for (provider, name, model, family) in [
        (
            NativeProvider::Claude,
            "claude",
            "claude-opus-5-5",
            "messages-family",
        ),
        (
            NativeProvider::Codex,
            "codex",
            "gpt-5.6-sol",
            "responses-family",
        ),
        (NativeProvider::Xai, "grok", "grok-4.6", "responses-family"),
    ] {
        for child in [false, true] {
            let authority = format!("previous-{name}-{}", if child { "leaf" } else { "root" });
            let mut tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
                "previous-release-tree",
                "root",
                "owner",
                PathBuf::from("/"),
                "selected-tuple",
                WriterMode::SingleLease,
            )
            .unwrap();
            let caller = if child {
                tree.spawn_agent(
                    "root",
                    &SpawnAgentRequest::test_named(
                        "review",
                        Tier::ReadOnly,
                        "reviewer",
                        "Review the synthetic fixture.",
                    ),
                    None,
                )
                .unwrap()
                .agent_id
            } else {
                "root".to_owned()
            };
            let extensions = crate::topology_extensions::topology_tool_extensions_in(
                Arc::new(Mutex::new(tree)),
                caller,
                if provider == NativeProvider::Xai {
                    crate::agent_tree_tools::TopologyVocabulary::TaskTools
                } else {
                    crate::agent_tree_tools::TopologyVocabulary::Plak
                },
            )?;
            let extras = extensions.names().cloned().collect::<Vec<_>>();
            let registry = coding_tool_registry_with_extensions(
                &authority,
                CompositionSelection::Direct,
                false,
                &extensions,
            )?;
            let process_bindings = registry.definitions().iter().filter_map(|definition| registry.identity(definition.name()))
                .filter(|identity| identity.executor_id().as_str() == "plakous.process-session-executor.v3")
                .map(|identity| serde_json::json!({"executor":identity.executor_id().as_str(),"binding":identity.executor_binding_identity().digest()}))
                .collect::<Vec<_>>();
            let selected = selected_coding_provider_configuration(
                provider,
                ModelId::new(model)?,
                ReasoningEffort::High,
                provider.enrolled_tool_mode(model),
                CompositionSelection::Direct,
            )?;
            let resolved = standard_model(&selected, family)?;
            let node = node_session(
                plakous::tools::JobConfig::default(),
                resolved,
                &registry,
                &extensions,
                &extras,
                SessionToolFence::Full,
                None,
            )?;
            let profile = selected_harness_profile(
                provider,
                &ModelId::new(model)?,
                ReasoningEffort::High,
                false,
                false,
                &extras,
                true,
                CompositionSelection::Direct,
                &node.surface,
                plakous::prompts::identity::PromptWorkspace::new(
                    Path::new("/"),
                    Path::new("/"),
                    "linux",
                ),
            )?
            .unwrap();
            let plan = plakous::agent::compile_explicit_runtime_profile(
                &profile,
                node.configured_model.model().as_ref(),
                &node.configuration,
                &registry,
                workspace.as_ref(),
                &plakous::tools::ExecutionBackendDescriptor::local_runner(),
                Vec::new(),
            )?;
            let selected_id = node.configuration.selected_identity();
            let effective_id = node.configuration.effective_identity();
            let plan_id = *plan.plan_digest();
            let builder = plakous::agent::AgentBuilder::new(
                node.configured_model,
                registry,
                Arc::clone(&workspace),
            )
            .projection_admission(Arc::new(crate::coding_routes::ProductProjectionAdmission))
            .tool_projection(node.surface.projection().clone())
            .harness_plan(plan)
            .identity_sources(
                Arc::new(plakous::model::FixedClock::new(
                    plakous::model::Timestamp::new(1, 0)?,
                )),
                Arc::new(plakous::model::SystemIdGenerator),
            )
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
            let CreateSessionOutcome::Created { session, .. } = sessions
                .create(CreateSessionRequest::new(
                    SessionOperationId::new(format!("fixture:{authority}"))?,
                    SessionRuntimeBinding::new(builder),
                ))
                .await?
            else {
                panic!("fixture create");
            };
            let locator = session.release().await?;
            cases.push(serde_json::json!({"provider":name,"model":model,"family":family,"child":child,
                "authority":authority,"locator":locator.to_string(),"selected":selected_id,"effective":effective_id,"plan":plan_id,"process_bindings":process_bindings}));
        }
    }
    for composition in [
        CompositionSelection::Direct,
        CompositionSelection::NativePtc,
        CompositionSelection::JsPtc,
        CompositionSelection::PyPtc,
    ] {
        for child in [false, true] {
            let authority = format!(
                "previous-openai-{}-{}",
                composition.as_str(),
                if child { "leaf" } else { "root" }
            );
            let mut tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
                "previous-api-tree",
                "root",
                "owner",
                PathBuf::from("/"),
                "selected-tuple",
                WriterMode::SingleLease,
            )
            .unwrap();
            let caller = if child {
                tree.spawn_agent(
                    "root",
                    &SpawnAgentRequest::test_named(
                        "orchestrate",
                        Tier::ReadOnly,
                        "supervisor",
                        "Review the synthetic fixture.",
                    ),
                    None,
                )
                .unwrap()
                .agent_id
            } else {
                "root".into()
            };
            let extensions = crate::topology_extensions::topology_tool_extensions(
                Arc::new(Mutex::new(tree)),
                caller,
            )?;
            let registry = coding_tool_registry_with_extensions(
                &authority,
                composition,
                composition == CompositionSelection::NativePtc,
                &extensions,
            )?;
            let process_bindings = registry.definitions().iter().filter_map(|definition| registry.identity(definition.name()))
            .filter(|identity| identity.executor_id().as_str() == "plakous.process-session-executor.v3")
            .map(|identity| serde_json::json!({"executor":identity.executor_id().as_str(),"binding":identity.executor_binding_identity().digest()})).collect::<Vec<_>>();
            let runtime = crate::platform::tests::previous_fixture_runtime(
                &extensions,
                Path::new("/"),
                &authority,
                false,
                child,
                composition,
            )
            .await?;
            let target = runtime.target_binding();
            let CreateSessionOutcome::Created { session, .. } = sessions
                .create(CreateSessionRequest::new(
                    SessionOperationId::new(format!("fixture:{authority}"))?,
                    runtime,
                ))
                .await?
            else {
                panic!("API fixture create")
            };
            let plan_id = *session.agent().harness_plan().plan_digest();
            let locator = session.release().await?;
            cases.push(serde_json::json!({"provider":"openai", "platform":true,"composition":composition.as_str(),"model":target.model().as_str(),"family":"responses-family", "child":child,
            "authority":authority,"locator":locator.to_string(),"selected":target.selected_provider_configuration(),
            "effective":target.effective_provider_configuration(),"plan":plan_id,"process_bindings":process_bindings}));
        }
    }
    for composition in [
        CompositionSelection::NativePtc,
        CompositionSelection::JsPtc,
        CompositionSelection::PyPtc,
    ] {
        let authority = format!("previous-native-{}", composition.as_str());
        let roots = ProductStateRoots::with_doubles(output.join(&authority), output.join("state"));
        let runtime = Box::pin(production_predecessor::production_predecessor_runtime(
            roots,
            &authority,
            composition,
            false,
        ))
        .await?;
        let target = runtime.target_binding();
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new(format!("fixture:{authority}"))?,
                runtime,
            ))
            .await?
        else {
            panic!("native production fixture create")
        };
        let plan = *session.agent().harness_plan().plan_digest();
        let locator = session.release().await?;
        cases.push(serde_json::json!({"production_native":true,"composition":composition.as_str(),"authority":authority,
            "locator":locator.to_string(),"selected":target.selected_provider_configuration(),"effective":target.effective_provider_configuration(),"plan":plan}));
    }
    std::fs::write(
        output.join("manifest.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "source_commit": std::env::var("PLAK_BINDING_SOURCE_COMMIT")?,
            "runtime_schema": plakous::sessions::RUNTIME_STORE_SCHEMA_VERSION,
            "runtime_epoch": plakous::sessions::RUNTIME_FORMAT_EPOCH,
            "platform": "linux", "workspace": "/", "cases":cases,
        }))?,
    )?;
    Ok(())
}
