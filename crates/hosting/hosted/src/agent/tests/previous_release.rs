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
    let inspected = plakous::sessions::rehearsal::inspect_binding_presentations_sqlite(&database)?;
    assert_eq!(inspected.len(), 6);
    for (_, frozen) in inspected {
        assert_eq!(crate::agent::check_frozen_binding(&frozen)?, None);
    }
    let sessions = AgentSessions::new(SessionBackend::sqlite(&database)?)?;
    let workspace = Arc::new(stamped_workspace_policy(
        Path::new("/"),
        Path::new(""),
        None,
    )?);
    for case in manifest["cases"].as_array().unwrap() {
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
        )?
        .with_frozen_presentation(Some(frozen));
        let extras = extensions.names().cloned().collect::<Vec<_>>();
        let registry = coding_tool_registry_with_extensions(
            authority,
            CompositionSelection::Direct,
            false,
            &extensions,
        )?;
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
        let builder = AgentBuilder::new(node.configured_model, registry, Arc::clone(&workspace))
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
        let opened = sessions
            .open(OpenSessionRequest::new(
                SessionOperationId::new(format!("candidate:{authority}"))?,
                locator,
                SessionRuntimeBinding::new(builder),
            ))
            .await;
        #[cfg(target_os = "linux")]
        {
            let OpenSessionOutcome::Opened { session, .. } = opened? else {
                panic!("predecessor exact open: {authority}");
            };
            assert_eq!(session.locator(), locator);
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
