use super::*;
use plakous::model::CallId;
use std::str::FromStr;

fn call_id(n: u128) -> CallId {
    CallId::from_str(&uuid::Uuid::from_u128(n).to_string()).expect("call id")
}

fn cell(cell_id: &str, sequence: u32) -> ToolCallerLineage {
    ToolCallerLineage::local_programmatic(
        41,
        7,
        cell_id,
        sequence,
        call_id(u128::from(sequence)),
        3,
    )
    .expect("lineage")
}

#[test]
fn direct_and_provider_lineage_are_not_cells() {
    assert!(CellIdentity::from_lineage(&ToolCallerLineage::Direct, "root").is_none());
    let provider = ToolCallerLineage::provider_programmatic(
        plakous::model::NamespaceId::new("hosted.program").expect("namespace"),
        "exec-1",
    )
    .expect("provider lineage");
    assert!(CellIdentity::from_lineage(&provider, "root").is_none());
}

#[test]
fn cell_identity_ignores_request_sequence_and_child_call_id() {
    let first = CellIdentity::from_lineage(&cell("cell-a", 1), "root").expect("cell");
    let second = CellIdentity::from_lineage(&cell("cell-a", 2), "root").expect("cell");
    assert_eq!(first, second);
    let other = CellIdentity::from_lineage(&cell("cell-b", 1), "root").expect("cell");
    assert_ne!(first, other);
}

#[test]
fn the_bound_is_lower_only() {
    assert_eq!(clamp_max_agents_per_cell(0), 1);
    assert_eq!(clamp_max_agents_per_cell(1), 1);
    assert_eq!(
        clamp_max_agents_per_cell(CEILING_MAX_AGENTS_PER_CELL),
        CEILING_MAX_AGENTS_PER_CELL
    );
    assert_eq!(
        clamp_max_agents_per_cell(u16::MAX),
        CEILING_MAX_AGENTS_PER_CELL
    );
    assert_eq!(
        CellAgentBudget::new(900).budget(),
        CEILING_MAX_AGENTS_PER_CELL
    );
    assert_eq!(CellAgentBudget::new(2).budget(), 2);
}

#[test]
fn the_ninth_admission_is_refused_and_a_second_cell_is_fresh() {
    let budget = CellAgentBudget::new(DEFAULT_MAX_AGENTS_PER_CELL);
    let first = CellIdentity::from_lineage(&cell("cell-a", 1), "root").expect("cell");
    for expected in 1..=DEFAULT_MAX_AGENTS_PER_CELL {
        assert_eq!(budget.admit_agent(&first).expect("admitted"), expected);
    }
    let refused = budget.admit_agent(&first).expect_err("ninth is refused");
    assert_eq!(refused.budget, DEFAULT_MAX_AGENTS_PER_CELL);
    assert_eq!(refused.used, DEFAULT_MAX_AGENTS_PER_CELL);
    assert_eq!(refused.cell_id, first.label());
    let body = refused.to_failure_json();
    assert_eq!(body["code"], "cell_budget_exhausted");
    assert_eq!(body["budget"], 8);
    assert_eq!(body["used"], 8);
    assert_eq!(body["cell_id"], "root/41/7/cell-a");

    let second = CellIdentity::from_lineage(&cell("cell-b", 1), "root").expect("cell");
    assert_eq!(budget.admit_agent(&second).expect("fresh cell"), 1);
}

/// B8: the same cell coordinates under two agents are two cells, so a root
/// cell and a child cell at the same operation id never share a counter.
#[test]
fn the_same_cell_under_another_caller_is_a_different_cell() {
    let budget = CellAgentBudget::new(1);
    let root = CellIdentity::from_lineage(&cell("cell", 1), "root").expect("cell");
    let child = CellIdentity::from_lineage(&cell("cell", 1), "c1").expect("cell");
    assert_ne!(root, child);
    assert_eq!(budget.admit_agent(&root).expect("root"), 1);
    assert_eq!(budget.admit_agent(&child).expect("child"), 1);
    assert!(budget.admit_agent(&root).is_err());
}

#[test]
fn a_later_interpreter_generation_is_a_different_cell() {
    let budget = CellAgentBudget::new(1);
    let first = CellIdentity::from_lineage(
        &ToolCallerLineage::local_programmatic(41, 7, "cell-a", 1, call_id(1), 3).expect("lineage"),
        "root",
    )
    .expect("cell");
    let later = CellIdentity::from_lineage(
        &ToolCallerLineage::local_programmatic(41, 8, "cell-a", 1, call_id(2), 3).expect("lineage"),
        "root",
    )
    .expect("cell");
    assert_eq!(budget.admit_agent(&first).expect("first"), 1);
    assert!(budget.admit_agent(&first).is_err());
    assert_eq!(budget.admit_agent(&later).expect("later generation"), 1);
}

/// the bound is epoch state. A snapshot below the ceiling
/// yields a budget at that bound; one above it is clamped; the budget is
/// not a process static that one config load can move under another tree.
#[test]
fn the_budget_is_read_from_the_epoch_snapshot() {
    let mut snapshot = OrchestrationSnapshot::unpinned("tuple");
    assert_eq!(
        CellAgentBudget::for_snapshot(&snapshot).budget(),
        CEILING_MAX_AGENTS_PER_CELL
    );
    snapshot.max_agents_per_cell = 3;
    assert_eq!(CellAgentBudget::for_snapshot(&snapshot).budget(), 3);
    snapshot.max_agents_per_cell = 4096;
    assert_eq!(
        CellAgentBudget::for_snapshot(&snapshot).budget(),
        CEILING_MAX_AGENTS_PER_CELL
    );
}
