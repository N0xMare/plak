//! The production move builder reads a finished caller's policy without opening it.
use super::*;
use crate::agent_tree::{MemoryTreeStore, TreeStore as _};
use crate::agent_tree_protocol::{SpawnAgentRequest, SubmitResultRequest, Tier, WriterMode};
use plakous::sessions::{
    AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend, SessionOperationId,
};

struct Trees(Arc<MemoryTreeStore>);
impl TreeStoreFactory for Trees {
    fn open(&self) -> CliResult<Arc<dyn crate::agent_tree::TreeStore>> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn production_move_builder_compiles_a_finished_child_without_open_or_grant_changes()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let authority = "passive-finished-child";
    let mut tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
        authority,
        "root",
        "owner",
        workspace.clone(),
        "selected",
        WriterMode::SingleLease,
    )
    .map_err(|error| error.message)?;
    let child = tree
        .spawn_agent(
            "root",
            &SpawnAgentRequest::test_named("review", Tier::ReadOnly, "reviewer", "return a result"),
            None,
        )
        .map_err(|error| error.message)?
        .agent_id;
    let store = Arc::new(MemoryTreeStore::new());
    let saved = store.save_in_custody(&tree).map_err(|error| error.detail)?;
    tree.acknowledge_revision(saved);
    let roots = ProductStateRoots::with_doubles(
        directory.path().join("product"),
        directory.path().join("state"),
    )
    .with_tree_store_factory(Arc::new(Trees(store.clone())));
    let mut spec = native_fixture_spec(&workspace, roots.clone(), authority)?;
    spec.product_home = Some(directory.path().join("home"));
    spec.plan_state_authority = format!("{authority}.{child}");
    spec.topology_caller_id = Some(child.clone());
    let runtime = build::coding_move_runtime_binding(spec.clone(), &mut Vec::new()).await?;
    let sessions = AgentSessions::new(SessionBackend::sqlite(
        directory.path().join("runtime.sqlite3"),
    )?)?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("finished-child:create")?,
            runtime,
        ))
        .await?
    else {
        return Err("source did not create".into());
    };
    let locator = session.release().await?;
    let frozen = Arc::new(sessions.created_tool_presentation(locator).await?);
    tree.submit_result_presented(
        &child,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: "{\"ok\":true}".into(),
        },
        None,
        |name| name,
    )
    .map_err(|error| error.message)?;
    tree.record_turn_completed(&child)
        .map_err(|error| error.message)?;
    store.save_in_custody(&tree).map_err(|error| error.detail)?;
    let status = tree.node(&child).map_err(|error| error.message)?.status;
    let grants = serde_json::to_value(tree.grants())?;
    let revision = tree.revision;
    assert!(
        !tree
            .node(&child)
            .map_err(|error| error.message)?
            .active_turn
    );
    spec.state_roots = roots.with_move_compilation_data(frozen, store.clone());
    let _runtime = build::coding_move_runtime_binding(spec, &mut Vec::new()).await?;
    let after = store
        .load(authority)
        .map_err(|error| error.detail)?
        .ok_or("tree missing")?;
    assert_eq!(after.rows().revision, revision);
    assert_eq!(
        after
            .rows()
            .node(&child)
            .map_err(|error| error.message)?
            .status,
        status
    );
    assert!(
        !after
            .rows()
            .node(&child)
            .map_err(|error| error.message)?
            .active_turn
    );
    assert_eq!(serde_json::to_value(after.rows().grants())?, grants);
    assert!(crate::topology_runtime::lookup(authority).is_none());
    assert_eq!(
        AgentSessions::export_history_sqlite(directory.path().join("runtime.sqlite3"), locator)?
            .segments(),
        []
    );
    Ok(())
}
