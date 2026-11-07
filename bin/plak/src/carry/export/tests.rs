use super::*;
use crate::home::paths::PlakPaths;
use crate::store::sessions::SessionRecord;
use plakous::model::Actor;
use plakous_host_protocol::topology::{NodeStatus, WorkspaceMode, WorkspaceStatus};
use std::fs;

#[test]
fn settled_export_matches_the_writable_projector_path() {
    use plakous::sessions::{
        CreateSessionOutcome, CreateSessionRequest, SessionBackend, SessionOperationId,
    };
    use plakous_hosted::session_store::SessionRecordStore;
    use plakous_offline_crash_harness::protocol::CrashBoundary;
    use plakous_offline_crash_harness::{BoundaryController, text_runtime_binding};

    let (home, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().unwrap();
    let mut document = crate::carry::document::sample_document();
    document.workspace_root = home.path().to_path_buf();
    let crate::carry::CarriedImport {
        mut record,
        history,
        ..
    } = document.into_import().unwrap();
    // A product create writes the genesis profile; this direct create stands in.
    record.root_profile = Some(plakous_hosted::sessions::RootProfile::default());
    let sessions =
        AgentSessions::new(SessionBackend::sqlite(paths.runtime_database()).unwrap()).unwrap();
    let binding = text_runtime_binding(
        home.path(),
        BoundaryController::quiet_from_evidence_path(
            CrashBoundary::None,
            home.path().join("evidence"),
        )
        .unwrap(),
        "synthetic",
        "synthetic",
    )
    .unwrap();
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let CreateSessionOutcome::Created { session, .. } = executor
        .block_on(
            sessions.create(
                CreateSessionRequest::new(
                    SessionOperationId::new(&record.create_operation_id).unwrap(),
                    binding,
                )
                .with_history(history),
            ),
        )
        .unwrap()
        .unwrap()
    else {
        panic!("new source session");
    };
    record.plakous_locator = Some(executor.block_on(session.release()).unwrap().unwrap());
    let records = crate::store::session_records::SqliteSessionStore::new(paths.clone());
    records.save(&mut record).unwrap();

    // Settle once, then exercise the public fast path and force the ordinary
    // writable projector path over exactly that same session.
    export_after_projection(&paths, &record).unwrap();
    let record = records.load(&record.id).unwrap();
    assert!(current_projection(&paths, &record).unwrap().is_some());
    let mut read_only = export_session(&paths, &record).unwrap();
    // Lifecycle metadata is home-local: neither overrides nor archival may
    // alter a v12 document, even a single serialized byte.
    let original_bytes = serde_json::to_vec(&read_only).unwrap();
    let mut metadata = crate::store::ProductStore::open(&paths).unwrap();
    metadata
        .set_title_override(&record.id, Some("operator label"))
        .unwrap();
    metadata.set_archived(&record.id, true).unwrap();
    let renamed = records.load(&record.id).unwrap();
    assert_eq!(
        serde_json::to_vec(&export_session(&paths, &renamed).unwrap()).unwrap(),
        original_bytes
    );
    assert_eq!(VERSION, 12);
    let imported = export_session(&paths, &renamed)
        .unwrap()
        .into_import()
        .unwrap()
        .record;
    assert_eq!(imported.title_override, None);
    assert!(!imported.archived);
    let mut writable = export_after_projection(&paths, &record).unwrap();
    read_only.local_digest = None;
    writable.local_digest = None;
    assert_eq!(
        serde_json::to_value(read_only).unwrap(),
        serde_json::to_value(writable).unwrap()
    );
}

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
