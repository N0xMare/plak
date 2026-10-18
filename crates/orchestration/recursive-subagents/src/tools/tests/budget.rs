//! The per-cell agent budget.

use super::*;

fn spawn_tool_with_budget(driver: TreeDriver, budget: Arc<CellAgentBudget>) -> TopologyTool {
    let mut tool = spawn_tool(driver.clone(), None);
    tool.cell_budget = budget;
    tool
}

fn failure_code(outcome: &ToolOutcome) -> String {
    outcome_json(outcome)
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn per_cell_agent_budget_denies_the_ninth_spawn() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tool = spawn_tool_with_budget(
        driver.clone(),
        Arc::new(CellAgentBudget::new(DEFAULT_MAX_AGENTS_PER_CELL)),
    );
    let arguments = serde_json::to_value(spawn_request()).unwrap();
    for sequence in 1..=u32::from(DEFAULT_MAX_AGENTS_PER_CELL) {
        let outcome = tool
            .dispatch(
                &cell_lineage("cell-a", sequence),
                format!("op-cell-a-{sequence}"),
                &arguments,
            )
            .await
            .unwrap();
        assert_ne!(
            failure_code(&outcome),
            "cell_budget_exhausted",
            "admission {sequence} of {DEFAULT_MAX_AGENTS_PER_CELL} must not be budget-refused"
        );
    }
    let ninth = tool
        .dispatch(&cell_lineage("cell-a", 9), "op-cell-a-9".into(), &arguments)
        .await
        .unwrap();
    assert_eq!(ninth.kind(), ToolOutcomeKind::Failure);
    let body = outcome_json(&ninth);
    assert_eq!(body["code"], "cell_budget_exhausted");
    assert_eq!(body["budget"], 8);
    assert_eq!(body["used"], 8);
    assert_eq!(body["cell_id"], "root/41/7/cell-a");
}

/// `purpose=followup` is new agent work, so it spends the
/// cell's budget; ordinary mail on a child the cell already has does not,
/// or an exhausted cell could not steer what it started.
#[tokio::test]
async fn followup_spends_the_cell_budget_and_ordinary_mail_does_not() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let mut tool = send_tool(driver.clone(), None);
    tool.cell_budget = Arc::new(CellAgentBudget::new(1));

    let mut request = serde_json::json!({ "agent_id": "c1", "message": "steer" });
    assert!(
        !tool.consumes_cell_agent_budget_for(&request),
        "ordinary mail rides an agent that already exists"
    );
    request["purpose"] = serde_json::json!("followup");
    assert!(tool.consumes_cell_agent_budget_for(&request));

    // The one unit this cell has goes to the followup; a second is refused
    // by the budget before the tree ever sees it.
    let first = tool
        .dispatch(&cell_lineage("cell-f", 1), "op-cell-f-1".into(), &request)
        .await
        .unwrap();
    assert_ne!(failure_code(&first), "cell_budget_exhausted");
    let second = tool
        .dispatch(&cell_lineage("cell-f", 2), "op-cell-f-2".into(), &request)
        .await
        .unwrap();
    assert_eq!(failure_code(&second), "cell_budget_exhausted");
    // Steering is still available to the exhausted cell.
    let steer = serde_json::json!({ "agent_id": "c1", "message": "steer" });
    let steered = tool
        .dispatch(&cell_lineage("cell-f", 3), "op-cell-f-3".into(), &steer)
        .await
        .unwrap();
    assert_ne!(failure_code(&steered), "cell_budget_exhausted");
}

#[tokio::test]
async fn nested_agent_budget_resets_per_cell() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let budget = Arc::new(CellAgentBudget::new(1));
    let tool = spawn_tool_with_budget(driver.clone(), Arc::clone(&budget));
    let arguments = serde_json::to_value(spawn_request()).unwrap();
    assert_ne!(
        failure_code(
            &tool
                .dispatch(&cell_lineage("cell-a", 1), "op-a-1".into(), &arguments)
                .await
                .unwrap()
        ),
        "cell_budget_exhausted"
    );
    assert_eq!(
        failure_code(
            &tool
                .dispatch(&cell_lineage("cell-a", 2), "op-a-2".into(), &arguments)
                .await
                .unwrap()
        ),
        "cell_budget_exhausted"
    );
    // A second cell in the same evaluation starts at zero.
    assert_ne!(
        failure_code(
            &tool
                .dispatch(&cell_lineage("cell-b", 1), "op-b-1".into(), &arguments)
                .await
                .unwrap()
        ),
        "cell_budget_exhausted"
    );
}

#[tokio::test]
async fn agent_budget_is_keyed_on_local_programmatic_lineage_not_operation_id() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tool = spawn_tool_with_budget(driver.clone(), Arc::new(CellAgentBudget::new(1)));
    let arguments = serde_json::to_value(spawn_request()).unwrap();
    // Every nested call carries a distinct derived child call id, so the
    // `operation_id` differs on each of these; the budget must still see
    // one cell. Keying on `operation_id` would give each call its own.
    tool.dispatch(
        &cell_lineage("cell-a", 1),
        "op-derived-1".into(),
        &arguments,
    )
    .await
    .unwrap();
    let second = tool
        .dispatch(
            &cell_lineage("cell-a", 2),
            "op-derived-2".into(),
            &arguments,
        )
        .await
        .unwrap();
    assert_eq!(failure_code(&second), "cell_budget_exhausted");
}

#[tokio::test]
async fn direct_topology_calls_bypass_the_cell_budget() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tool = spawn_tool_with_budget(driver.clone(), Arc::new(CellAgentBudget::new(1)));
    let arguments = serde_json::to_value(spawn_request()).unwrap();
    for operation in 1..=3 {
        let outcome = tool
            .dispatch(
                &ToolCallerLineage::Direct,
                format!("op-direct-{operation}"),
                &arguments,
            )
            .await
            .unwrap();
        assert_ne!(failure_code(&outcome), "cell_budget_exhausted");
    }
    // A hosted-program caller is not a cell either.
    let provider = ToolCallerLineage::provider_programmatic(
        plakous::model::NamespaceId::new("hosted.program").unwrap(),
        "exec-1",
    )
    .unwrap();
    let outcome = tool
        .dispatch(&provider, "op-provider-1".into(), &arguments)
        .await
        .unwrap();
    assert_ne!(failure_code(&outcome), "cell_budget_exhausted");
}

#[tokio::test]
async fn every_code_mode_variant_hits_the_same_budget_check() {
    // native-ptc, js-ptc and py-ptc share this host-call dispatch: L0
    // stamps the same LocalProgrammatic lineage whichever interpreter
    // produced the nested request, so the three differ only in `cell_id`.
    for cell_id in ["native-ptc-cell-1", "js-ptc-cell-1", "py-ptc-cell-1"] {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tool = spawn_tool_with_budget(driver.clone(), Arc::new(CellAgentBudget::new(1)));
        let arguments = serde_json::to_value(spawn_request()).unwrap();
        tool.dispatch(
            &cell_lineage(cell_id, 1),
            format!("op-{cell_id}-1"),
            &arguments,
        )
        .await
        .unwrap();
        let refused = tool
            .dispatch(
                &cell_lineage(cell_id, 2),
                format!("op-{cell_id}-2"),
                &arguments,
            )
            .await
            .unwrap();
        assert_eq!(
            failure_code(&refused),
            "cell_budget_exhausted",
            "{cell_id} must be budgeted by the same host check"
        );
        assert_eq!(
            outcome_json(&refused)["cell_id"],
            format!("root/41/7/{cell_id}")
        );
    }
}

#[tokio::test]
async fn an_exhausted_cell_can_still_wait_list_and_close() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let budget = Arc::new(CellAgentBudget::new(1));
    let spawn = spawn_tool_with_budget(driver.clone(), Arc::clone(&budget));
    let arguments = serde_json::to_value(spawn_request()).unwrap();
    spawn
        .dispatch(
            &cell_lineage("cell-a", 1),
            "op-exhaust-1".into(),
            &arguments,
        )
        .await
        .unwrap();
    assert_eq!(
        failure_code(
            &spawn
                .dispatch(
                    &cell_lineage("cell-a", 2),
                    "op-exhaust-2".into(),
                    &arguments
                )
                .await
                .unwrap()
        ),
        "cell_budget_exhausted"
    );
    let mut close = close_tool(driver.clone(), None);
    close.cell_budget = Arc::clone(&budget);
    let closed = close
        .dispatch(
            &cell_lineage("cell-a", 3),
            "op-exhaust-close".into(),
            &agent_id_request("c1"),
        )
        .await
        .unwrap();
    assert_eq!(closed.kind(), ToolOutcomeKind::Success);
}

#[test]
fn one_cell_budget_is_shared_by_every_topology_tool_on_a_tree() {
    // The extension set is rebuilt per caller and per turn; a counter owned
    // by one set would reset under the cell and enforce nothing. The tree
    // owns it, per epoch.
    let root = tempfile::tempdir().unwrap();
    let tree = bootstrap_tree("tree-w4-shared", root.path());
    let left = tree.cell_budget();
    let right = tree.cell_budget();
    assert!(Arc::ptr_eq(&left, &right));
    assert!(left.budget() <= CEILING_MAX_AGENTS_PER_CELL);
}
