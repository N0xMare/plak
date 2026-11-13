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

#[tokio::test]
#[ignore = "explicit production-size read-only inspection measurement"]
async fn binding_inspection_timing_for_two_hundred_checkpoint_heads()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("runtime.sqlite3");
    let workspace = Arc::new(stamped_workspace_policy(
        directory.path(),
        Path::new(""),
        None,
    )?);
    let sessions = AgentSessions::new(SessionBackend::sqlite(&database)?)?;
    let extensions = crate::tools::ProductToolExtensions::empty();
    let registry = coding_tool_registry_with_extensions(
        PLAN_AUTHORITY,
        CompositionSelection::Direct,
        false,
        &extensions,
    )?;
    let selected = selected_coding_provider_configuration(
        NativeProvider::Claude,
        ModelId::new("claude-opus-5-5")?,
        ReasoningEffort::High,
        None,
        CompositionSelection::Direct,
    )?;
    for ordinal in 0..200 {
        let node = node_session(
            plakous::tools::JobConfig::default(),
            standard_claude_model(&selected)?,
            &registry,
            &extensions,
            &[],
            SessionToolFence::Full,
            None,
        )?;
        let history = plakous::sessions::PortableSessionHistory::new(
            Vec::new(),
            "00".repeat(32),
            ModelId::new("claude-opus-5-5")?,
        )
        .with_named_state(std::collections::BTreeMap::from([(
            "retained_data".into(),
            serde_json::json!(format!("{ordinal}:{}", "x".repeat(128 * 1024))),
        )]))?;
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(
                CreateSessionRequest::new(
                    SessionOperationId::new(format!("inspection-size:{ordinal}"))?,
                    builder(node, registry.clone(), workspace.clone(), true),
                )
                .with_history(history),
            )
            .await?
        else {
            panic!("create inspection fixture")
        };
        session.release().await?;
    }
    let started = std::time::Instant::now();
    let heads = plakous::sessions::rehearsal::inspect_binding_presentations_sqlite(&database)?;
    let mut output = Vec::new();
    for head in heads {
        assert_eq!(
            crate::agent::check_frozen_binding(&head.presentation.unwrap()?)?,
            None
        );
        output.push(serde_json::json!({"session":head.locator.session_id().to_string(), "branch":head.locator.branch_id().to_string(),
            "checkpoint":head.locator.checkpoint_id().to_string(), "archived":false,"superseded":false,
            "binding_change":null,"unavailable":null,"unaffected":[]}));
    }
    eprintln!(
        "binding inspection: {} heads, {} database bytes, {} report bytes, {:?}",
        output.len(),
        std::fs::metadata(database)?.len(),
        serde_json::to_vec(&output)?.len(),
        started.elapsed()
    );
    Ok(())
}

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
    previous_extensions(extensions, composition, incompatible, None)
}

fn previous_extensions(
    extensions: &crate::tools::ProductToolExtensions,
    composition: CompositionSelection,
    incompatible: bool,
    executor_override: Option<&str>,
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
            match executor_override {
                Some(id) => plakous::tools::ToolExecutorId::new(id)?,
                None => identity.executor_id().clone(),
            },
            if incompatible {
                ToolExecutorBindingIdentity::new(1, &[b"incompatible"])?
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

pub(crate) fn frozen_native_fixture_runtime(
    extensions: &crate::tools::ProductToolExtensions,
    workspace: &Path,
    authority: &str,
) -> Result<SessionRuntimeBinding, Box<dyn std::error::Error>> {
    let registry = coding_tool_registry_with_extensions(
        authority,
        CompositionSelection::Direct,
        false,
        extensions,
    )?;
    let selected = selected_coding_provider_configuration(
        NativeProvider::Claude,
        ModelId::new("claude-opus-5-5")?,
        ReasoningEffort::High,
        None,
        CompositionSelection::Direct,
    )?;
    let extras = extensions.names().cloned().collect::<Vec<_>>();
    let node = node_session(
        plakous::tools::JobConfig::default(),
        standard_claude_model(&selected)?,
        &registry,
        extensions,
        &extras,
        SessionToolFence::Full,
        None,
    )?;
    Ok(builder(
        node,
        registry,
        Arc::new(stamped_workspace_policy(workspace, Path::new(""), None)?),
        true,
    ))
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
        assert!(inspected.iter().any(|head| head.locator == locator));
        for head in inspected {
            if head.locator != locator {
                continue;
            }
            if let Some(presentation) = head.presentation {
                assert_eq!(crate::agent::check_frozen_binding(&presentation?)?, None);
            }
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
        let changed_selected = selected_coding_provider_configuration(
            NativeProvider::Claude,
            ModelId::new("claude-opus-5-5")?,
            ReasoningEffort::Low,
            None,
            CompositionSelection::Direct,
        )?;
        let changed = standard_claude_model(&changed_selected)?;
        let refusal = frozen
            .restore_configuration(changed.configuration())
            .expect_err("frozen tools must not mask a changed reasoning selection");
        assert_eq!(
            refusal.binding_change(),
            Some(&plakous::agent::BindingChange::ProviderConfiguration)
        );
        let refusal = node_session(
            plakous::tools::JobConfig::default(),
            changed,
            &registry,
            &restored,
            &extras,
            SessionToolFence::Full,
            None,
        )
        .err()
        .expect("frozen reopen construction must reject changed non-tool configuration");
        let refusal = std::error::Error::source(&refusal)
            .and_then(|error| error.downcast_ref::<plakous::sessions::AgentSessionsError>())
            .expect("construction must preserve its typed binding refusal");
        assert_eq!(
            refusal.binding_change(),
            Some(&plakous::agent::BindingChange::ProviderConfiguration)
        );
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
        let inspected =
            plakous::sessions::rehearsal::inspect_binding_presentations_sqlite(&database)?;
        let old_head = inspected
            .iter()
            .find(|head| {
                head.locator.session_id() == locator.session_id()
                    && head.locator.branch_id() == locator.branch_id()
            })
            .unwrap();
        assert!(!old_head.line_head);
        assert!(
            old_head.presentation.as_ref().unwrap().is_ok(),
            "superseded bindings must still be decoded and reported"
        );
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
        let changed_node = node_session(
            plakous::tools::JobConfig::default(),
            standard_claude_model(&selected)?,
            &live,
            &incompatible,
            &extras,
            SessionToolFence::Full,
            None,
        )?;
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new(format!("revised-executor:{child}"))?,
                builder(changed_node, live, Arc::clone(&workspace), true),
            ))
            .await?
        else {
            panic!("create revised executor fixture")
        };
        let locator = session.release().await?;
        assert!(matches!(
            crate::agent::check_frozen_binding(
                &sessions.created_tool_presentation(locator).await?
            )?,
            Some(plakous::agent::BindingChange::ToolBinding { .. })
        ));
        for external_executor in [
            "fixture.unknown.send.executor",
            "external-executor",
            "external.executor",
        ] {
            let unknown = previous_extensions(
                &current,
                CompositionSelection::Direct,
                false,
                Some(external_executor),
            )?;
            let registry = coding_tool_registry_with_extensions(
                PLAN_AUTHORITY,
                CompositionSelection::Direct,
                false,
                &unknown,
            )?;
            let node = node_session(
                plakous::tools::JobConfig::default(),
                standard_claude_model(&selected)?,
                &registry,
                &unknown,
                &extras,
                SessionToolFence::Full,
                None,
            )?;
            let CreateSessionOutcome::Created { session, .. } = sessions
                .create(CreateSessionRequest::new(
                    SessionOperationId::new(format!(
                        "unknown-executor:{child}:{external_executor}"
                    ))?,
                    builder(node, registry, Arc::clone(&workspace), true),
                ))
                .await?
            else {
                panic!("create unknown executor fixture")
            };
            let locator = session.release().await?;
            let unavailable = crate::agent::check_frozen_binding(
                &sessions.created_tool_presentation(locator).await?,
            )
            .unwrap_err();
            assert!(
                unavailable
                    .message()
                    .contains("cannot establish current executor authority")
            );
        }
        for removed_executor in [
            "plakous.standard.removed-executor.v1",
            "plakous.merge-plan.removed-executor.v1",
            "plakous.process.removed-executor.v1",
        ] {
            let missing_release = previous_extensions(
                &current,
                CompositionSelection::Direct,
                false,
                Some(removed_executor),
            )?;
            let registry = coding_tool_registry_with_extensions(
                PLAN_AUTHORITY,
                CompositionSelection::Direct,
                false,
                &missing_release,
            )?;
            let node = node_session(
                plakous::tools::JobConfig::default(),
                standard_claude_model(&selected)?,
                &registry,
                &missing_release,
                &extras,
                SessionToolFence::Full,
                None,
            )?;
            let CreateSessionOutcome::Created { session, .. } = sessions
                .create(CreateSessionRequest::new(
                    SessionOperationId::new(format!("missing-release:{child}:{removed_executor}"))?,
                    builder(node, registry, Arc::clone(&workspace), true),
                ))
                .await?
            else {
                panic!("create missing release executor fixture")
            };
            let locator = session.release().await?;
            assert!(matches!(
                crate::agent::check_frozen_binding(
                    &sessions.created_tool_presentation(locator).await?
                )?,
                Some(plakous::agent::BindingChange::ToolBinding { .. })
            ));
            sessions.archive(locator).await?;
            let inspected =
                plakous::sessions::rehearsal::inspect_binding_presentations_sqlite(&database)?;
            let archived = inspected
                .into_iter()
                .find(|head| head.locator == locator)
                .unwrap();
            assert!(archived.archived);
            assert!(
                matches!(
                    crate::agent::check_frozen_binding(&archived.presentation.unwrap()?)?,
                    Some(plakous::agent::BindingChange::ToolBinding { .. })
                ),
                "archived heads retain their actual non-gating verdict"
            );
        }
    }
    Ok(())
}
