//! Opt-in startup timing over a synthetic home with large, varied tree ledgers.

use super::*;
use plakous_hosted::agent_tree_protocol::{SpawnAgentRequest, WriterMode};
use plakous_hosted::occupancy_vcs::fake::FakeVcs;
use rusqlite::params;
use sha2::{Digest as _, Sha256};
use std::sync::Arc;

fn seed_tree(
    paths: &crate::home::paths::PlakPaths,
    record: &crate::store::sessions::SessionRecord,
    handoff: bool,
) {
    let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
        Arc::new(FakeVcs::new()),
        &record.id,
        "root",
        "owner",
        record.workspace_root.clone(),
        "selected-tuple",
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
    if handoff {
        tree.record_handoff().unwrap();
    }
    crate::store::tree::save_tree(paths, &mut tree).unwrap();
}

fn add_ledgers(paths: &crate::home::paths::PlakPaths, ids: &[String]) {
    let mut connection = rusqlite::Connection::open(paths.subagents_database()).unwrap();
    let transaction = connection.transaction().unwrap();
    {
        let mut messages = transaction.prepare("INSERT INTO agent_messages(tree_id,message_id,from_agent,to_agent,body,purpose,priority,disposition,durable) VALUES (?1,?2,'root','c1',?3,'coordinate','deferred','queued','terminal')").unwrap();
        let mut operations = transaction.prepare("INSERT INTO agent_operations(tree_id,operation_id,caller_agent_id,tool_name,canonical_request,request_digest,terminal_response) VALUES (?1,?2,'root','send_agent_message',?3,?4,?5)").unwrap();
        let body = "synthetic message ".repeat(256);
        let request =
            serde_json::json!({"agent_id":"c1", "message":"synthetic request ".repeat(384)})
                .to_string();
        let digest =
            Sha256::digest(request.as_bytes())
                .iter()
                .fold(String::new(), |mut text, byte| {
                    use std::fmt::Write as _;
                    write!(text, "{byte:02x}").unwrap();
                    text
                });
        for (index, id) in ids.iter().enumerate() {
            let message_count = 64 * (index % 4 + 1);
            let operation_count = 32 * (4 - index % 4);
            for row in 0..message_count {
                messages
                    .execute(params![id, format!("m{}", row + 1), body])
                    .unwrap();
            }
            for row in 0..operation_count {
                let response = serde_json::json!({"message_id":format!("synthetic-message-{row}"), "disposition":"queued"}).to_string();
                operations
                    .execute(params![
                        id,
                        format!("synthetic-operation-{row}"),
                        request,
                        digest,
                        response
                    ])
                    .unwrap();
            }
        }
    }
    transaction.commit().unwrap();
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
}

#[test]
#[ignore = "manual scale measurement, not a timing gate"]
fn startup_candidate_scan_timing() {
    let (_temporary, paths) = crate::home::paths::PlakPaths::isolated_test_home();
    let services = Services::for_home(paths.clone());
    let mut product = ProductStore::open(&paths).unwrap();
    let mut ids = Vec::new();
    for index in 0..200 {
        let mut record = crate::store::sessions::SessionRecord::new(paths.home().join("workspace"));
        product.commit(&mut record).unwrap();
        seed_tree(&paths, &record, index % 50 == 0);
        ids.push(record.id);
    }
    add_ledgers(&paths, &ids);
    let bytes = std::fs::metadata(paths.subagents_database()).unwrap().len();
    let sessions = ProductSessions::new(&services, None, None).unwrap();
    eprintln!("scan timing: trees=200 handoffs=4 messages=32000 operations=16000 db_bytes={bytes}");
    for round in 1..=5 {
        let started = std::time::Instant::now();
        let owed = sessions.owed().unwrap();
        let elapsed = started.elapsed();
        assert_eq!(owed.len(), 4);
        eprintln!(
            "scan timing: round={round} owed=4 elapsed_us={}",
            elapsed.as_micros()
        );
    }
}
