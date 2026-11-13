//! A predecessor-authored native store, never rewritten by the candidate to fit.

use super::{
    CompositionSelection, SessionToolFence, coding_tool_registry_with_extensions, node_session,
    selected_coding_provider_configuration, stamped_workspace_policy, standard_model,
};
use crate::agent_tree_protocol::{SpawnAgentRequest, Tier, WriterMode};
use plakous::agent::{
    AgentBuilder, AgentRequestPolicy, CapabilityRequirement, GenerationLimits, ReasoningEffort,
    ReasoningPolicy,
};
use plakous::model::{FixedClock, ModelId, SystemIdGenerator, Timestamp};
#[cfg(target_os = "linux")]
use plakous::sessions::OpenSessionOutcome;
use plakous::sessions::{
    AgentSessions, OpenSessionRequest, SessionBackend, SessionLocator, SessionOperationId,
    SessionRuntimeBinding,
};
use plakous::subscriptions::NativeProvider;
use sha2::{Digest as _, Sha256};
use std::path::Path;
use std::sync::{Arc, Mutex};

struct NeverInvoked;
impl plakous::tools::ToolExecutor for NeverInvoked {
    fn execute(
        self: Arc<Self>,
        _: plakous::tools::ToolInvocation,
        _: plakous::tools::ToolContext,
    ) -> plakous::tools::ToolFuture {
        panic!("a predecessor binding witness must never execute")
    }
}

fn manifest_bindings(
    live: &plakous::tools::FrozenToolRegistry,
    case: &serde_json::Value,
) -> Result<plakous::tools::FrozenToolRegistry, Box<dyn std::error::Error>> {
    let mut builder = plakous::tools::ToolRegistryBuilder::default();
    let executor: Arc<dyn plakous::tools::ToolExecutor> = Arc::new(NeverInvoked);
    for definition in live.definitions() {
        let identity = live.identity(definition.name()).unwrap();
        let binding = if identity.executor_id().as_str() == "plakous.process-session-executor.v3" {
            let recorded = case["process_bindings"]
                .as_array()
                .ok_or("manifest process bindings missing")?
                .first()
                .ok_or("manifest process binding absent")?;
            assert_eq!(recorded["executor"], identity.executor_id().as_str());
            plakous::tools::ToolExecutorBindingIdentity::from_digest(serde_json::from_value(
                recorded["binding"].clone(),
            )?)
        } else {
            identity.executor_binding_identity()
        };
        let mut registration = plakous::tools::ToolRegistration::new(
            identity.tool_id().clone(),
            identity.executor_id().clone(),
            binding,
            definition.clone(),
            executor.clone(),
        )
        .with_owner(identity.owner().clone())
        .nested_eligible();
        if identity.claims_presented_name() {
            registration = registration.presented_name_claimer();
        }
        builder.register(registration)?;
    }
    Ok(builder.freeze())
}

async fn open_api_case(
    sessions: &AgentSessions,
    case: &serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let locator: SessionLocator = case["locator"].as_str().unwrap().parse()?;
    let frozen = Arc::new(sessions.created_tool_presentation(locator).await?);
    let mut tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
        "previous-api-tree",
        "root",
        "owner",
        "/".into(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let child = case["child"].as_bool().unwrap();
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
    let extensions =
        crate::topology_extensions::topology_tool_extensions(Arc::new(Mutex::new(tree)), caller)?
            .with_frozen_presentation(Some(frozen.clone()));
    let authority = case["authority"].as_str().unwrap();
    let composition =
        CompositionSelection::parse(case["composition"].as_str().unwrap_or("direct"))?;
    #[cfg(not(target_os = "linux"))]
    if composition != CompositionSelection::Direct {
        assert_eq!(crate::agent::check_frozen_binding(&frozen)?, None);
        return Ok(());
    }
    let runtime = crate::platform::tests::previous_fixture_runtime(
        &extensions,
        Path::new("/"),
        authority,
        true,
        child,
        composition,
    )
    .await?;
    assert_eq!(
        serde_json::to_value(runtime.target_binding().selected_provider_configuration())?,
        case["selected"]
    );
    assert_eq!(
        serde_json::to_value(runtime.target_binding().effective_provider_configuration())?,
        case["effective"]
    );
    let opened = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new(format!("candidate:{authority}"))?,
            locator,
            runtime,
        ))
        .await;
    #[cfg(target_os = "linux")]
    {
        let OpenSessionOutcome::Opened { session, .. } = opened? else {
            panic!("API predecessor open")
        };
        let compact = crate::platform::tests::previous_fixture_runtime(
            &extensions,
            Path::new("/"),
            authority,
            true,
            child,
            composition,
        )
        .await?
        .with_compaction_output_limit()?;
        let answer = sessions
            .compact(plakous::sessions::CompactSessionRequest::new(
                SessionOperationId::new(format!("candidate-compact:{authority}"))?,
                locator,
                plakous::sessions::CompactReason::Manual,
                compact,
            ))
            .await;
        match answer {
            Ok(plakous::sessions::CompactSessionOutcome::NoEligibleRange { .. }) => {}
            Err(error) => {
                assert!(
                    child,
                    "a root fixture must admit unpaid measurement: {error}"
                );
                assert_eq!(error.binding_change(), None);
                assert!(
                    error
                        .to_string()
                        .contains("compaction source measurement failed"),
                    "{error}"
                );
                assert!(
                    error
                        .to_string()
                        .contains("Platform request intent is not admitted"),
                    "{error}"
                );
            }
            other => panic!("unexpected predecessor compact answer: {other:?}"),
        }
        session.release().await?;
    }
    #[cfg(not(target_os = "linux"))]
    assert_eq!(
        opened
            .err()
            .ok_or("Linux fixture unexpectedly opened")?
            .binding_change(),
        Some(&plakous::agent::BindingChange::ExecutionEnvironment)
    );
    Ok(())
}

async fn open_production_native_case(
    sessions: &AgentSessions,
    case: &serde_json::Value,
    directory: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let locator: SessionLocator = case["locator"].as_str().unwrap().parse()?;
    let frozen = sessions.created_tool_presentation(locator).await?;
    assert_eq!(
        serde_json::to_value(frozen.plan().plan_digest())?,
        case["plan"]
    );
    assert_eq!(crate::agent::check_frozen_binding(&frozen)?, None);
    #[cfg(target_os = "linux")]
    {
        let roots = crate::agent::ProductStateRoots::with_doubles(
            directory.join("product"),
            directory.join("state"),
        )
        .for_exact_reopen(sessions, locator)
        .await?;
        let composition = CompositionSelection::parse(case["composition"].as_str().unwrap())?;
        let authority = case["authority"].as_str().unwrap();
        let runtime = Box::pin(
            super::production_predecessor::production_predecessor_runtime(
                roots,
                authority,
                composition,
                true,
            ),
        )
        .await?;
        assert_eq!(
            serde_json::to_value(runtime.target_binding().selected_provider_configuration())?,
            case["selected"]
        );
        assert_eq!(
            serde_json::to_value(runtime.target_binding().effective_provider_configuration())?,
            case["effective"]
        );
        let OpenSessionOutcome::Opened { session, .. } = sessions
            .open(OpenSessionRequest::new(
                SessionOperationId::new(format!("candidate:{authority}"))?,
                locator,
                runtime,
            ))
            .await?
        else {
            panic!("native production predecessor reopen")
        };
        assert_eq!(session.release().await?, locator);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = directory;
    Ok(())
}

#[tokio::test]
async fn previous_build_native_bindings_reopen_without_rebinding_or_rewriting_identities()
-> Result<(), Box<dyn std::error::Error>> {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/previous-release");
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixtures.join("manifest.json"))?)?;
    assert_eq!(
        manifest["source_commit"],
        "a40a209ff426e167583b41debbf7eb8fe63aa564"
    );
    assert_eq!(
        manifest["runtime_schema"],
        plakous::sessions::RUNTIME_STORE_SCHEMA_VERSION
    );
    assert_eq!(
        manifest["runtime_epoch"],
        plakous::sessions::RUNTIME_FORMAT_EPOCH
    );
    let bytes = std::fs::read(fixtures.join("previous-runtime.sqlite3"))?;
    assert_eq!(
        plakous::model::hex_lower(Sha256::digest(&bytes).as_slice()),
        manifest["sha256"].as_str().unwrap()
    );
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("runtime.sqlite3");
    std::fs::write(&database, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o600))?;
    }
    let inspected =
        plakous::sessions::rehearsal::inspect_binding_presentations_sqlite(&database, &[])?;
    assert_eq!(inspected.len(), manifest["cases"].as_array().unwrap().len());
    for head in inspected {
        let frozen = head.presentation.unwrap()?;
        assert_eq!(crate::agent::check_frozen_binding(&frozen)?, None);
    }
    let sessions = AgentSessions::new(SessionBackend::sqlite(&database)?)?;
    let workspace = Arc::new(stamped_workspace_policy(
        Path::new("/"),
        Path::new(""),
        None,
    )?);
    for case in manifest["cases"].as_array().unwrap() {
        if case["production_native"].as_bool() == Some(true) {
            Box::pin(open_production_native_case(
                &sessions,
                case,
                directory.path(),
            ))
            .await?;
            continue;
        }
        if case["platform"].as_bool() == Some(true) {
            Box::pin(open_api_case(&sessions, case)).await?;
            continue;
        }
        let provider = match case["provider"].as_str().unwrap() {
            "claude" => NativeProvider::Claude,
            "codex" => NativeProvider::Codex,
            "grok" => NativeProvider::Xai,
            _ => panic!("unknown fixture provider"),
        };
        let model = case["model"].as_str().unwrap();
        let child = case["child"].as_bool().unwrap();
        let authority = case["authority"].as_str().unwrap();
        let locator: SessionLocator = case["locator"].as_str().unwrap().parse()?;
        let frozen = Arc::new(sessions.created_tool_presentation(locator).await?);
        assert_eq!(
            serde_json::to_value(frozen.configuration().selected_identity())?,
            case["selected"]
        );
        assert_eq!(
            serde_json::to_value(frozen.configuration().effective_identity())?,
            case["effective"]
        );
        assert_eq!(
            serde_json::to_value(frozen.plan().plan_digest())?,
            case["plan"]
        );
        let mut tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
            "previous-release-tree",
            "root",
            "owner",
            "/".into(),
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
        let live = coding_tool_registry_with_extensions(
            authority,
            CompositionSelection::Direct,
            false,
            &extensions,
        )?;
        let registry = frozen.restore_registry(&manifest_bindings(&live, case)?)?;
        let extensions = extensions.with_frozen_presentation(Some(frozen));
        let selected = selected_coding_provider_configuration(
            provider,
            ModelId::new(model)?,
            ReasoningEffort::High,
            provider.enrolled_tool_mode(model),
            CompositionSelection::Direct,
        )?;
        let configured = standard_model(&selected, case["family"].as_str().unwrap())?;
        let node = node_session(
            plakous::tools::JobConfig::default(),
            configured,
            &registry,
            &extensions,
            &extras,
            SessionToolFence::Full,
            None,
        )?;
        let bound = node
            .configuration
            .effective()
            .behavior()
            .output_bound
            .bound();
        let runtime = || -> Result<SessionRuntimeBinding, Box<dyn std::error::Error>> {
            let builder = AgentBuilder::new(
                node.configured_model.clone(),
                registry.clone(),
                Arc::clone(&workspace),
            )
            .projection_admission(Arc::new(crate::coding_routes::ProductProjectionAdmission))
            .tool_projection(node.surface.projection().clone())
            .identity_sources(
                Arc::new(FixedClock::new(Timestamp::new(1, 0)?)),
                Arc::new(SystemIdGenerator),
            )
            .request_policy(AgentRequestPolicy::new(
                GenerationLimits::new(bound, Vec::new())?,
                ReasoningPolicy::Enabled {
                    effort: Some(ReasoningEffort::High),
                    requirement: CapabilityRequirement::Required,
                },
            ));
            Ok(SessionRuntimeBinding::new(builder))
        };
        let opened = sessions
            .open(OpenSessionRequest::new(
                SessionOperationId::new(format!("candidate:{authority}"))?,
                locator,
                runtime()?,
            ))
            .await;
        #[cfg(target_os = "linux")]
        {
            let OpenSessionOutcome::Opened { session, .. } = opened? else {
                panic!("predecessor exact open: {authority}");
            };
            assert_eq!(session.locator(), locator);
            let compact = runtime()?.with_compaction_output_limit()?;
            let answer = sessions
                .compact(plakous::sessions::CompactSessionRequest::new(
                    SessionOperationId::new(format!("candidate-compact:{authority}"))?,
                    locator,
                    plakous::sessions::CompactReason::Manual,
                    compact,
                ))
                .await;
            // The native fixture's non-invoked model deliberately has no
            // Streaming capability. Reaching measurement proves its frozen
            // compact runtime passed admission, without preparing paid work.
            let error = answer.expect_err("fixture measurement must remain non-dispatching");
            assert_eq!(error.binding_change(), None);
            assert!(
                error
                    .to_string()
                    .contains("compaction source measurement failed"),
                "{error}"
            );
            assert_eq!(session.release().await?, locator);
        }
        #[cfg(not(target_os = "linux"))]
        {
            let error = opened
                .err()
                .ok_or("a Linux local-runner fixture must not open on another OS")?;
            assert_eq!(
                error.binding_change(),
                Some(&plakous::agent::BindingChange::ExecutionEnvironment)
            );
        }
    }
    Ok(())
}
