use super::*;
use crate::home::paths::PlakPaths;
use crate::store::sessions::SessionRecord;
use plakous::model::Actor;
use plakous_host_protocol::topology::{NodeStatus, WorkspaceMode, WorkspaceStatus};
use std::fs;

#[test]
fn readable_spellings_pin_every_variant_to_the_previous_debug_output() {
    for actor in [
        Actor::Developer,
        Actor::System,
        Actor::User,
        Actor::Assistant,
        Actor::Tool,
        Actor::Harness,
    ] {
        assert_eq!(actor.as_str(), format!("{actor:?}").to_ascii_lowercase());
    }
    for status in [
        NodeStatus::Pending,
        NodeStatus::Running,
        NodeStatus::Paused,
        NodeStatus::Sealing,
        NodeStatus::Completed,
        NodeStatus::Interrupted,
        NodeStatus::Failed,
        NodeStatus::Closed,
        NodeStatus::RecoveryRequired,
    ] {
        assert_eq!(node_status_name(status), format!("{status:?}"));
    }
    for status in [
        WorkspaceStatus::Active,
        WorkspaceStatus::Adopting,
        WorkspaceStatus::Discarding,
        WorkspaceStatus::CleanRemoved,
        WorkspaceStatus::Adopted,
        WorkspaceStatus::Discarded,
    ] {
        assert_eq!(workspace_status_name(status), format!("{status:?}"));
    }
    for mode in [
        WorkspaceMode::Inherit,
        WorkspaceMode::Snapshot,
        WorkspaceMode::Fork,
    ] {
        assert_eq!(workspace_mode_name(mode), format!("{mode:?}"));
    }
}

#[test]
fn inert_children_are_archivable_but_live_or_sealing_children_refuse() {
    assert_eq!(
        count_archivable_children(
            [
                ("a", NodeStatus::Completed),
                ("b", NodeStatus::Failed),
                ("c", NodeStatus::Closed),
                ("d", NodeStatus::RecoveryRequired),
                ("e", NodeStatus::Interrupted),
            ]
            .into_iter()
        ),
        Ok(5)
    );
    for status in [
        NodeStatus::Pending,
        NodeStatus::Running,
        NodeStatus::Paused,
        NodeStatus::Sealing,
    ] {
        assert_eq!(
            count_archivable_children([("live", status)].into_iter()),
            Err(ExportRefusal::SubagentRunning {
                agent_id: "live".to_owned(),
                status
            })
        );
    }
}

#[test]
fn tree_reference_facts_omit_settled_rows_and_count_them() {
    use plakous_hosted::agent_tree_protocol::{AttemptScopedRequest, SpawnAgentRequest};
    let (_home, paths) = PlakPaths::isolated_test_home();
    let repo = plakous_recursive_subagents::tree::vcs_fixtures::git_repo();
    paths.ensure_layout().unwrap();
    let record = SessionRecord::new(repo.path().to_path_buf());
    let mut tree = plakous_hosted::topology_runtime::test_support::bootstrap(
        &record.id,
        repo.path().to_path_buf(),
    );
    let completed = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap();
    let closed = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap();
    tree.record_turn_completed(&completed.agent_id).unwrap();
    tree.close_agent(
        "root",
        &AttemptScopedRequest {
            agent_id: closed.agent_id.clone(),
            expected_attempt_id: None,
        },
        None,
    )
    .unwrap();
    crate::store::tree::save_tree(&paths, &mut tree).unwrap();
    let (count, resolved, facts) = tree_reference_facts(&paths, &record).unwrap();
    assert_eq!(count, 2);
    assert!(facts.is_empty());
    assert_eq!(resolved, 3);
    fs::write(repo.path().join("uncommitted.txt"), "retained changes").unwrap();
    let (_, _, facts) = tree_reference_facts(&paths, &record).unwrap();
    assert!(facts.iter().any(|entry| entry.kind == "source_workspace"));
    assert!(!facts.iter().any(|entry| entry.kind == "source_child"));
}

#[test]
fn closed_child_dirty_copy_remains_in_reference_ledger() {
    use plakous_hosted::agent_tree_protocol::{AttemptScopedRequest, SpawnAgentRequest, Tier};
    let (_home, paths) = PlakPaths::isolated_test_home();
    let repo = plakous_recursive_subagents::tree::vcs_fixtures::git_repo();
    paths.ensure_layout().unwrap();
    let record = SessionRecord::new(repo.path().to_path_buf());
    let mut tree = plakous_hosted::topology_runtime::test_support::bootstrap(
        &record.id,
        repo.path().to_path_buf(),
    );
    let child = tree
        .spawn_agent(
            "root",
            &SpawnAgentRequest::test_named("implement", Tier::Mutate, "editor", "edit").with_fork(),
            None,
        )
        .unwrap();
    fs::write(
        tree.node(&child.agent_id)
            .unwrap()
            .path
            .join("retained.txt"),
        "retained work",
    )
    .unwrap();
    tree.close_agent(
        "root",
        &AttemptScopedRequest {
            agent_id: child.agent_id.clone(),
            expected_attempt_id: None,
        },
        None,
    )
    .unwrap();
    tree.settle_for_test();
    crate::store::tree::save_tree(&paths, &mut tree).unwrap();
    let (_, _, facts) = tree_reference_facts(&paths, &record).unwrap();
    assert!(
        facts
            .iter()
            .any(|entry| entry.kind == "source_workspace" && entry.operation == child.workspace.id)
    );
    assert!(facts.iter().any(|entry| entry.kind == "source_candidate"));
    assert!(!facts.iter().any(|entry| entry.kind == "source_child"));
}

#[test]
fn a_command_or_workflow_intent_without_a_fact_refuses_export() {
    let root = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("home"));
    let mut record = SessionRecord::new(root.path().to_path_buf());
    record.plakous_locator = Some(test_locator());
    let projected =
        |commands: &[&str], workflows: &[&str]| plakous_hosted::runtime::ProjectedSession {
            head: test_locator(),
            pending_commands: commands.iter().map(|id| (*id).to_owned()).collect(),
            pending_workflows: workflows.iter().map(|id| (*id).to_owned()).collect(),
        };
    let refusal =
        export_projected(&paths, &record, &projected(&["plak:command"], &[])).unwrap_err();
    assert_eq!(refusal, ExportRefusal::UnsettledIntent);
    assert!(
        refusal
            .to_string()
            .contains("open the session once to settle it")
    );
    assert_eq!(
        export_projected(&paths, &record, &projected(&[], &["plak:compact"])).unwrap_err(),
        ExportRefusal::UnsettledIntent
    );
}

fn test_locator() -> plakous::sessions::SessionLocator {
    "00000000-0000-0000-0000-000000000001/\
     00000000-0000-0000-0000-000000000002/\
     00000000-0000-0000-0000-000000000003"
        .parse()
        .unwrap()
}
