use super::*;
use crate::tools::TreeDriver;
use crate::tree::{
    MemoryTreeStore, OperationRecord, TreeStore,
    test_support::{fake_tree, spawn_req},
};
use std::sync::{Arc, Mutex};

#[test]
fn an_idle_fold_still_joins_its_unprojected_session_allocation() {
    let (_tmp, _vcs, mut tree) = fake_tree();
    let child = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    let locator = "00000000-0000-4000-8000-000000000100/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001";
    tree.record_child_session(&child, locator).unwrap();
    assert!(tree.node(&child).unwrap().session_locator.is_none());
    tree.fold_scratch_clones = 0;
    tree.fold_due(&child).unwrap();
    assert_eq!(
        tree.node(&child).unwrap().session_locator.as_deref(),
        Some(locator)
    );
    assert_eq!(tree.fold_scratch_clones, 1);
    tree.fold_due(&child).unwrap();
    assert_eq!(tree.fold_scratch_clones, 1);
}

#[test]
fn idle_folds_and_only_deleted_close_targets_need_no_scratch_clone() {
    let (_tmp, _vcs, mut tree) = fake_tree();
    let child = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    tree.fold_scratch_clones = 0;
    let before = format!("{tree:?}");
    assert_eq!(tree.fold_due(&child).unwrap(), [] as [IntentJoinRefusal; 0]);
    assert_eq!(format!("{tree:?}"), before);
    tree.nodes.get_mut(&child).unwrap().fold_order = None;
    tree.pending_close_folds.insert(child.clone());
    let before = format!("{tree:?}");
    assert_eq!(
        tree.fold_reserved_closes().unwrap(),
        [] as [IntentJoinRefusal; 0]
    );
    assert_eq!(format!("{tree:?}"), before);
    assert_eq!(tree.fold_scratch_clones, 0);
}

#[test]
fn resume_tree_propagates_a_fold_integrity_refusal() {
    let (_tmp, _vcs, mut tree) = fake_tree();
    let id = "malformed-vcs-fault".to_owned();
    tree.operations.insert(
        id.clone(),
        OperationRecord {
            operation_id: id,
            caller_agent_id: "root".to_owned(),
            tool_name: crate::tree::workspaces::VCS_FAULT_KIND.to_owned(),
            canonical_request: "malformed".to_owned(),
            request_digest: String::new(),
            fold: Some(FoldAnchor {
                seq: 0,
                order: tree.revision,
            }),
            terminal_code: None,
            terminal_response: Some("effects_reserved".to_owned()),
        },
    );
    assert_eq!(
        tree.resume_tree().unwrap_err().code,
        TopologyErrorCode::OperationRecoveryRequired
    );
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "page fold intent index changed in its due range")]
fn page_index_rejects_a_new_same_agent_due_intent() {
    let (_tmp, _vcs, mut tree) = fake_tree();
    let index = tree.index_fold_intents("root");
    let id = "new-due".to_owned();
    tree.operations.insert(
        id.clone(),
        OperationRecord {
            operation_id: id,
            caller_agent_id: "root".to_owned(),
            tool_name: "interrupt".to_owned(),
            canonical_request: String::new(),
            request_digest: String::new(),
            fold: Some(FoldAnchor {
                seq: 0,
                order: tree.revision,
            }),
            terminal_code: None,
            terminal_response: None,
        },
    );
    let _ = tree.fold_agent_due_indexed("root", Some(&index));
}

#[test]
fn fenced_integrity_violation_restores_raw_and_driver_boundaries() {
    for corruption in ["count", "operation"] {
        for driver_boundary in [false, true] {
            let (_tmp, vcs, mut tree) = fake_tree();
            let id = "injected-fenced".to_owned();
            tree.operations.insert(
                id.clone(),
                OperationRecord {
                    operation_id: id,
                    caller_agent_id: "root".to_owned(),
                    tool_name: "test_fenced_violation".to_owned(),
                    canonical_request: corruption.to_owned(),
                    request_digest: String::new(),
                    fold: Some(FoldAnchor {
                        seq: 0,
                        order: tree.revision,
                    }),
                    terminal_code: None,
                    terminal_response: Some("effects_reserved".to_owned()),
                },
            );
            let store = Arc::new(MemoryTreeStore::new());
            tree.acknowledge_revision(store.save_in_custody(&tree).unwrap());
            let before = format!("{tree:?}");
            let error = if driver_boundary {
                let driver = TreeDriver::new(Arc::new(Mutex::new(tree.clone())), store, vcs, None);
                driver
                    .fold_and_commit(&mut tree, |tree| tree.fold_due_in_place("root"))
                    .unwrap_err()
            } else {
                tree.fold_due("root").unwrap_err()
            };
            assert_eq!(error.code, TopologyErrorCode::OperationRecoveryRequired);
            assert_eq!(format!("{tree:?}"), before);
        }
    }
}
