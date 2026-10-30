use std::error::Error;

use super::*;

#[test]
fn canonical_update_plan_schema_matches_main() {
    let expected = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "plan": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "step": {"type": "string"},
                        "status": {
                            "type": "string",
                            "enum": ["pending", "in_progress", "completed"]
                        }
                    },
                    "required": ["step", "status"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["plan"],
        "additionalProperties": false
    });
    assert_eq!(PlanToolPack::update_plan_input_schema(), expected);
}

#[test]
fn invalid_plan_content_is_a_model_visible_failure_not_a_lost_dispatch() {
    let error = ToolRuntimeError::new(
        ToolRuntimeErrorKind::InvalidArguments,
        "plan may contain at most one in-progress item",
    );
    let outcome = model_visible_plan_content_error(error).expect("failure text");
    assert_eq!(outcome.kind(), plakous_tools::ToolOutcomeKind::Failure);
}

#[test]
fn grok_oauth_todo_merge_flips_status_by_id_only() -> Result<(), Box<dyn Error>> {
    let mut id_steps = BTreeMap::from([("a".to_owned(), "step".to_owned())]);
    let current = vec![PlanItem::new("step", PlanItemStatus::Pending)?];
    let todos = vec![json!({"id": "a", "status": "completed"})];
    let plan = merge_todos_into_plan(true, &todos, &current, &mut id_steps)?;
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].step(), "step");
    assert_eq!(plan[0].status(), PlanItemStatus::Completed);
    Ok(())
}

#[test]
fn grok_oauth_todo_merge_fails_closed_on_cold_map_with_existing_snapshot() {
    let mut id_steps = BTreeMap::new();
    let current = vec![PlanItem::new("step", PlanItemStatus::Pending).expect("item")];
    let todos = vec![json!({"id": "a", "status": "completed"})];
    let error = merge_todos_into_plan(true, &todos, &current, &mut id_steps)
        .expect_err("cold merge must fail closed");
    assert_eq!(error.kind(), ToolRuntimeErrorKind::InvalidArguments);
    assert_eq!(error.message().as_str(), PLAKOUS_TOOL_MERGE_PLAN_COLD_MAP);
}

#[test]
fn grok_oauth_todo_replace_on_cold_map_with_existing_snapshot() -> Result<(), Box<dyn Error>> {
    let mut id_steps = BTreeMap::new();
    let current = vec![PlanItem::new("old step", PlanItemStatus::Pending)?];
    let todos = vec![json!({
        "id": "new",
        "content": "new step",
        "status": "pending"
    })];
    let plan = merge_todos_into_plan(false, &todos, &current, &mut id_steps)?;
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].step(), "new step");
    assert_eq!(id_steps.get("new").map(String::as_str), Some("new step"));
    Ok(())
}

#[test]
fn grok_oauth_todo_merge_false_replaces_existing_plan() -> Result<(), Box<dyn Error>> {
    let mut id_steps = BTreeMap::from([("old".to_owned(), "old step".to_owned())]);
    let current = vec![PlanItem::new("old step", PlanItemStatus::Pending)?];
    let todos = vec![json!({
        "id": "new",
        "content": "new step",
        "status": "in_progress"
    })];
    let plan = merge_todos_into_plan(false, &todos, &current, &mut id_steps)?;
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].step(), "new step");
    assert_eq!(plan[0].status(), PlanItemStatus::InProgress);
    assert!(!id_steps.contains_key("old"));
    assert_eq!(id_steps.get("new").map(String::as_str), Some("new step"));
    Ok(())
}

#[test]
fn grok_oauth_todo_cancelled_removes_item() -> Result<(), Box<dyn Error>> {
    let mut id_steps = BTreeMap::from([("a".to_owned(), "step".to_owned())]);
    let current = vec![PlanItem::new("step", PlanItemStatus::Pending)?];
    let todos = vec![json!({"id": "a", "status": "cancelled"})];
    let err = merge_todos_into_plan(true, &todos, &current, &mut id_steps)
        .expect_err("cancelled-only update must not leave an empty plan");
    assert_eq!(err.kind(), ToolRuntimeErrorKind::InvalidArguments);
    assert!(!id_steps.contains_key("a"));
    Ok(())
}

#[test]
fn public_plan_snapshot_construction_revalidates_private_items() {
    let invalid_steps = [String::new(), "é".repeat(MAX_STANDARD_PLAN_STEP_BYTES)];
    for step in invalid_steps {
        let result = PlanSnapshot::new(
            1,
            None,
            vec![PlanItem {
                step,
                status: PlanItemStatus::Pending,
            }],
        );
        assert_eq!(
            result.err().map(|error| error.kind()),
            Some(ToolRuntimeErrorKind::ResourceLimit)
        );
    }
}

#[tokio::test]
async fn plan_state_is_revisioned_and_conflict_safe() -> Result<(), ToolRuntimeError> {
    let plan_state = InMemoryPlanState::new();
    let first = plan_state
        .replace(PlanReplacement::new(
            Some(0),
            Some(String::from("start")),
            vec![PlanItem::new("work", PlanItemStatus::InProgress)?],
        )?)
        .await?;
    assert_eq!(first.revision(), 1);
    let conflict = plan_state
        .replace(PlanReplacement::new(
            Some(0),
            None,
            vec![PlanItem::new("done", PlanItemStatus::Completed)?],
        )?)
        .await;
    assert_eq!(
        conflict.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::Execution)
    );
    assert_eq!(plan_state.snapshot()?.revision(), 1);
    Ok(())
}
