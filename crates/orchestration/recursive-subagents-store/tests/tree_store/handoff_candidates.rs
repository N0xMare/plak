//! Candidate discovery is read-only and never replaces continuation admission.
#![cfg(feature = "test-support")]
#![allow(clippy::unwrap_used, reason = "deterministic offline law fixtures")]

use std::path::Path;
use std::sync::Arc;

use plakous_recursive_subagents::protocol::{AttemptScopedRequest, SpawnAgentRequest, WriterMode};
use plakous_recursive_subagents::tree::{
    FakeVcs, LossContinuationState, ProductAgentTree, TreeStore,
};
use plakous_recursive_subagents_store::SqliteTreeStore;

fn parked(root: &Path, id: &str) -> (ProductAgentTree, String) {
    let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
        Arc::new(FakeVcs::new()),
        id,
        "root",
        "owner",
        root.to_owned(),
        "tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap()
        .agent_id;
    tree.bind_session_locator(&child, "locator").unwrap();
    tree.set_active_turn(&child, true).unwrap();
    tree.bind_attempt_turn(&child, "00000000-0000-0000-0000-000000000001")
        .unwrap();
    tree.release_attempt_turn(&child).unwrap();
    tree.park_child(&child, Vec::new()).unwrap();
    (tree, child)
}

#[test]
fn handoff_candidates_include_held_and_pending_but_not_later_interrupts_or_running_nodes() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("subagents.sqlite3");
    let store = SqliteTreeStore::open(&path).unwrap();
    for id in ["held", "pending", "parent", "running", "lost", "parked"] {
        let (mut tree, child) = parked(root.path(), id);
        if id == "lost" {
            let _ = store.save_in_custody(&tree).unwrap();
            tree = ProductAgentTree::reopen(tree.to_persisted());
        } else if id != "parked" {
            tree.record_handoff().unwrap();
            match id {
                "held" => assert_eq!(
                    tree.loss_continuation(&child).unwrap().state,
                    LossContinuationState::Held
                ),
                "pending" => {
                    tree.record_loss_continuation(&child, 1).unwrap();
                    assert_eq!(
                        tree.loss_continuation(&child).unwrap().state,
                        LossContinuationState::Pending
                    );
                }
                "parent" => {
                    tree.interrupt_agent(
                        "root",
                        &AttemptScopedRequest {
                            agent_id: child.clone(),
                            expected_attempt_id: None,
                        },
                        None,
                    )
                    .unwrap();
                    assert_eq!(tree.loss_continuation(&child), None);
                }
                "running" => tree.set_active_turn(&child, true).unwrap(),
                _ => unreachable!(),
            }
        }
        let _ = store.save_in_custody(&tree).unwrap();
    }
    let connection = rusqlite::Connection::open(&path).unwrap();
    let before: i64 = connection
        .query_row("PRAGMA data_version", [], |row| row.get(0))
        .unwrap();
    for _ in 0..2 {
        assert_eq!(
            store.handoff_candidate_tree_ids().unwrap(),
            ["held", "pending"]
        );
    }
    let after: i64 = connection
        .query_row("PRAGMA data_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        before, after,
        "candidate discovery must not write, including custody"
    );
}

#[test]
fn handoff_candidates_do_not_decode_unrelated_ledger_payloads() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("subagents.sqlite3");
    let store = SqliteTreeStore::open(&path).unwrap();
    let (mut tree, child) = parked(root.path(), "candidate");
    tree.record_handoff().unwrap();
    let _ = store.save_in_custody(&tree).unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    // A malformed unrelated payload makes a full row decode fail; the
    // candidate prefilter must neither read it nor grant continuation itself.
    connection.execute("INSERT INTO agent_operations(tree_id,operation_id,caller_agent_id,tool_name,canonical_request,request_digest) VALUES ('candidate','unrelated',?1,'send_agent_message','not json','unused')", [&child]).unwrap();
    connection.execute("INSERT INTO agent_messages(tree_id,message_id,from_agent,to_agent,body,purpose,priority,disposition,durable) VALUES ('candidate','invalid-message-id','root',?1,'unrelated','coordinate','deferred','queued','terminal')", [&child]).unwrap();
    assert_eq!(store.handoff_candidate_tree_ids().unwrap(), ["candidate"]);
    assert!(store.load("candidate").is_err());
}
