//! Exact opens retain data while executors remain current and independently checked.

use super::{
    SessionToolFence, coding_tool_registry_with_extensions, node_session,
    selected_coding_provider_configuration, stamped_workspace_policy, standard_claude_model,
};
use crate::binding::CompositionSelection;
use plakous::agent::{CapabilityRequirement, ReasoningEffort};
use plakous::model::ModelId;
use plakous::model::{ToolDefinition, ToolInputContract};
use plakous::sessions::{
    AgentSessions, CreateSessionOutcome, CreateSessionRequest, OpenSessionOutcome,
    OpenSessionRequest, SessionBackend, SessionOperationId, SessionRuntimeBinding,
};
use plakous::subscriptions::NativeProvider;
use plakous::tools::{ToolExecutor, ToolExecutorBindingIdentity, ToolRegistration};
use std::path::Path;
use std::sync::{Arc, Mutex};

struct NeverInvoked;

const PLAN_AUTHORITY: &str = "00000000-0000-4000-8000-000000000011";

impl ToolExecutor for NeverInvoked {
    fn execute(
        self: Arc<Self>,
        _: plakous::tools::ToolInvocation,
        _: plakous::tools::ToolContext,
    ) -> plakous::tools::ToolFuture {
        panic!("presentation reconstruction must not execute tools")
    }
}

pub(crate) fn previous_send_extensions(
    extensions: &crate::tools::ProductToolExtensions,
    composition: CompositionSelection,
    incompatible: bool,
) -> Result<crate::tools::ProductToolExtensions, Box<dyn std::error::Error>> {
    let registry =
        coding_tool_registry_with_extensions(PLAN_AUTHORITY, composition, false, extensions)?;
    let mut registrations = Vec::new();
    for registration in extensions.registrations() {
        if registration.name().as_str() != "send_agent_message" {
            registrations.push(registration.clone());
            continue;
        }
        let definition = registration.definition();
        let ToolInputContract::Structured(schema) = definition.input() else {
            panic!("send is structured");
        };
        let mut document: serde_json::Value =
            serde_json::from_slice(schema.document().canonical_bytes())?;
        document["properties"]
            .as_object_mut()
            .unwrap()
            .remove("acknowledge_recovery");
        let previous = ToolDefinition::new(
            definition.name().clone(),
            definition.semantic_version(),
            "Frozen send presentation.",
            ToolInputContract::structured(document)?,
            definition.output().clone(),
            definition.concurrency(),
            definition.effect(),
        )?;
        let identity = registry.identity(definition.name()).unwrap();
        let replacement = ToolRegistration::new(
            identity.tool_id().clone(),
            identity.executor_id().clone(),
            if incompatible {
                ToolExecutorBindingIdentity::new(2, &[b"incompatible"])?
            } else {
                identity.executor_binding_identity()
            },
            previous,
            Arc::new(NeverInvoked),
        )
        .nested_eligible();
        registrations.push(replacement);
    }
    Ok(crate::tools::ProductToolExtensions::from_nested_registrations(registrations)?)
}

fn builder(
    node: crate::agent::build::NodeSession,
    registry: plakous::tools::FrozenToolRegistry,
    workspace: Arc<plakous::tools::WorkspacePolicy>,
    fresh: bool,
) -> SessionRuntimeBinding {
    let bound = node
        .configuration
        .effective()
        .behavior()
        .output_bound
        .bound();
    let plan = fresh.then(|| {
        let extras = registry
            .definitions()
            .iter()
            .map(|definition| definition.name().clone())
            .collect::<Vec<_>>();
        let profile = super::selected_harness_profile(
            NativeProvider::Claude,
            &ModelId::new("claude-opus-5-5").unwrap(),
            ReasoningEffort::High,
            false,
            false,
            &extras,
            true,
            CompositionSelection::Direct,
            &node.surface,
            plakous::prompts::identity::PromptWorkspace::new(
                workspace.root(),
                workspace.cwd(),
                std::env::consts::OS,
            ),
        )
        .unwrap()
        .unwrap();
        plakous::agent::compile_explicit_runtime_profile(
            &profile,
            node.configured_model.model().as_ref(),
            &node.configuration,
            &registry,
            workspace.as_ref(),
            &plakous::tools::ExecutionBackendDescriptor::local_runner(),
            Vec::new(),
        )
        .unwrap()
    });
    let mut builder = plakous::agent::AgentBuilder::new(node.configured_model, registry, workspace)
        .projection_admission(Arc::new(crate::coding_routes::ProductProjectionAdmission))
        .tool_projection(node.surface.projection().clone())
        .request_policy(plakous::agent::AgentRequestPolicy::new(
            plakous::agent::GenerationLimits::new(bound, Vec::new()).unwrap(),
            plakous::agent::ReasoningPolicy::Enabled {
                effort: Some(ReasoningEffort::High),
                requirement: CapabilityRequirement::Required,
            },
        ));
    if let Some(plan) = plan {
        builder = builder.harness_plan(plan);
    }
    SessionRuntimeBinding::new(builder)
}

#[tokio::test]
async fn frozen_direct_presentations_reopen_roots_and_leaves_without_digest_changes()
-> Result<(), Box<dyn std::error::Error>> {
    use crate::agent_tree_protocol::{SpawnAgentRequest, Tier, WriterMode};
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(stamped_workspace_policy(
        &std::fs::canonicalize(directory.path())?,
        Path::new(""),
        None,
    )?);
    let sessions = AgentSessions::new(SessionBackend::sqlite(
        directory.path().join("runtime.sqlite3"),
    )?)?;
    for child in [false, true] {
        let mut tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
            "frozen-tree",
            "root",
            "owner",
            directory.path().to_path_buf(),
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
                    "review the diff",
                ),
                None,
            )
            .unwrap()
            .agent_id
        } else {
            "root".to_owned()
        };
        let current = crate::topology_extensions::topology_tool_extensions(
            Arc::new(Mutex::new(tree)),
            caller,
        )?;
        let previous = previous_send_extensions(&current, CompositionSelection::Direct, false)?;
        let registry = coding_tool_registry_with_extensions(
            PLAN_AUTHORITY,
            CompositionSelection::Direct,
            false,
            &previous,
        )?;
        let selected = selected_coding_provider_configuration(
            NativeProvider::Claude,
            ModelId::new("claude-opus-5-5")?,
            ReasoningEffort::High,
            None,
            CompositionSelection::Direct,
        )?;
        let resolved = standard_claude_model(&selected)?;
        let extras = previous.names().cloned().collect::<Vec<_>>();
        let node = node_session(
            plakous::tools::JobConfig::default(),
            resolved.clone(),
            &registry,
            &previous,
            &extras,
            SessionToolFence::Full,
            None,
        )?;
        let identities = (
            node.configuration.selected_identity(),
            node.configuration.effective_identity(),
        );
        let created = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new(format!("frozen:{child}"))?,
                builder(node, registry.clone(), Arc::clone(&workspace), true),
            ))
            .await?;
        let CreateSessionOutcome::Created { session, .. } = created else {
            panic!("created");
        };
        let locator = session.locator();
        let database = directory.path().join("runtime.sqlite3");
        let before = std::fs::read(&database)?;
        let wal = directory.path().join("runtime.sqlite3-wal");
        let before_wal = std::fs::read(&wal).ok();
        let inspected =
            plakous::sessions::rehearsal::inspect_binding_presentations_sqlite(&database)?;
        assert!(inspected.iter().any(|(candidate, _)| *candidate == locator));
        for (_, presentation) in &inspected {
            assert_eq!(crate::agent::check_frozen_binding(presentation)?, None);
        }
        assert_eq!(std::fs::read(&database)?, before);
        assert_eq!(std::fs::read(&wal).ok(), before_wal);
        session.release().await?;
        let frozen = Arc::new(sessions.created_tool_presentation(locator).await?);
        let roots = crate::agent::ProductStateRoots::with_doubles(
            directory.path().to_path_buf(),
            directory.path().join("state"),
        )
        .for_exact_reopen(&sessions, locator)
        .await?;
        assert!(roots.frozen_presentation().is_some());
        assert!(
            roots
                .without_frozen_presentation()
                .frozen_presentation()
                .is_none()
        );
        let current_registry = coding_tool_registry_with_extensions(
            PLAN_AUTHORITY,
            CompositionSelection::Direct,
            false,
            &current,
        )?;
        assert_ne!(
            registry.registry_identity_digest()?,
            current_registry.registry_identity_digest()?
        );
        let fresh = node_session(
            plakous::tools::JobConfig::default(),
            resolved.clone(),
            &current_registry,
            &current,
            &extras,
            SessionToolFence::Full,
            None,
        )?;
        assert_ne!(fresh.configuration.selected_identity(), identities.0);
        let refusal = sessions
            .open(OpenSessionRequest::new(
                SessionOperationId::new(format!("unfrozen:{child}"))?,
                locator,
                builder(
                    fresh,
                    current_registry.clone(),
                    Arc::clone(&workspace),
                    false,
                ),
            ))
            .await
            .expect_err("current presentation must not silently rebind old history");
        assert_eq!(
            refusal.binding_change(),
            Some(&plakous::agent::BindingChange::ToolPresentation)
        );
        let restored = current
            .clone()
            .with_frozen_presentation(Some(frozen.clone()));
        let registry = coding_tool_registry_with_extensions(
            PLAN_AUTHORITY,
            CompositionSelection::Direct,
            false,
            &restored,
        )?;
        let node = node_session(
            plakous::tools::JobConfig::default(),
            resolved,
            &registry,
            &restored,
            &extras,
            SessionToolFence::Full,
            None,
        )?;
        assert_eq!(
            identities,
            (
                node.configuration.selected_identity(),
                node.configuration.effective_identity()
            )
        );
        assert_eq!(
            registry
                .definition(&plakous::model::ToolName::new("send_agent_message")?)
                .unwrap()
                .description(),
            "Frozen send presentation."
        );
        let opened = sessions
            .open(OpenSessionRequest::new(
                SessionOperationId::new(format!("reopen:{child}"))?,
                locator,
                builder(node, registry, Arc::clone(&workspace), false),
            ))
            .await?;
        let OpenSessionOutcome::Opened { session, .. } = opened else {
            panic!("exact open");
        };
        assert_eq!(session.locator(), locator);
        session.release().await?;
        let fresh = node_session(
            plakous::tools::JobConfig::default(),
            standard_claude_model(&selected)?,
            &current_registry,
            &current,
            &extras,
            SessionToolFence::Full,
            None,
        )?;
        let fresh_identity = fresh.configuration.selected_identity();
        let runtime = builder(fresh, current_registry, Arc::clone(&workspace), true);
        let rebound = Box::pin(sessions.rebind(
            plakous::sessions::RebindSessionRequest::recompiling_target(
                SessionOperationId::new(format!("rebind-current:{child}"))?,
                locator,
                runtime.target_binding(),
                runtime,
            ),
        ))
        .await?;
        let plakous::sessions::RebindSessionOutcome::Rebound { target, .. } = rebound else {
            panic!("explicit rebind must admit current presentation: {rebound:?}");
        };
        assert_eq!(target.session_id(), locator.session_id());
        let rebound = sessions.created_tool_presentation(target).await?;
        assert_eq!(rebound.configuration().selected_identity(), fresh_identity);
        assert_ne!(fresh_identity, identities.0);
        let incompatible = previous_send_extensions(&current, CompositionSelection::Direct, true)?;
        let live = coding_tool_registry_with_extensions(
            PLAN_AUTHORITY,
            CompositionSelection::Direct,
            false,
            &incompatible,
        )?;
        assert!(matches!(
            frozen.restore_registry(&live),
            Err(plakous::tools::ToolBindingError::BindingIdentityMismatch { .. })
        ));
    }
    Ok(())
}
