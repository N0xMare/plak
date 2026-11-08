//! Every legal raw read has its own real-wire read-only assertion.

use super::*;

fn snapshot(fixture: &Fixture) -> Vec<(String, Vec<String>)> {
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let database = rusqlite::Connection::open_with_flags(
        paths.product_database(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    database.execute_batch("BEGIN").unwrap();
    let mut snapshots: Vec<_> = [
        "sessions",
        "queue_items",
        "queue_attachments",
        "prompt_history",
        "submissions",
        "workflow_requests",
        "operator_notices",
        "host_intents",
        "projector_cursors",
        "session_views",
        "session_call_views",
        "session_open_turn_views",
        "session_process_views",
        "transcript_rows",
    ]
    .into_iter()
    .map(|table| {
        let mut query = database.prepare(&format!("SELECT * FROM {table}")).unwrap();
        let columns = query.column_count();
        let mut rows = query
            .query_map([], |row| {
                (0..columns)
                    .map(|index| row.get::<_, rusqlite::types::Value>(index))
                    .collect::<Result<Vec<_>, _>>()
                    .map(|values| format!("{values:?}"))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows.sort();
        (table.to_owned(), rows)
    })
    .collect();
    let tree = rusqlite::Connection::open_with_flags(
        paths.subagents_database(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    tree.execute_batch("BEGIN").unwrap();
    let mut tables = tree
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap();
    for table in tables.query_map([], |row| row.get::<_, String>(0)).unwrap() {
        let table = table.unwrap();
        let mut query = tree
            .prepare(&format!(
                "SELECT * FROM \"{}\"",
                table.replace('\"', "\"\"")
            ))
            .unwrap();
        let columns = query.column_count();
        let mut rows = query
            .query_map([], |row| {
                (0..columns)
                    .map(|index| row.get::<_, rusqlite::types::Value>(index))
                    .collect::<Result<Vec<_>, _>>()
                    .map(|values| format!("{values:?}"))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows.sort();
        snapshots.push((format!("tree:{table}"), rows));
    }
    snapshots
}

fn is_chrome(frame: &Value) -> bool {
    frame["frame"] == "event"
        && frame["observation"]["kind"] == "extension"
        && frame["observation"]["body"]
            .get("workspace_chrome")
            .is_some()
}

fn audits() -> [Value; 4] {
    [
        include_str!("../../../../fixtures/client-traces/host-control-stop-root-reads-audit.json"),
        include_str!("../../../../fixtures/client-traces/host-control-stop-child-reads-audit.json"),
        include_str!(
            "../../../../fixtures/client-traces/host-control-switch-root-reads-audit.json"
        ),
        include_str!(
            "../../../../fixtures/client-traces/host-control-switch-child-reads-audit.json"
        ),
    ]
    .map(|document| serde_json::from_str(document).unwrap())
}

#[test]
fn every_legal_raw_read_is_acknowledged_without_session_queue_transcript_or_tree_changes() {
    let fixture = fixture().unwrap();
    cold_script(&fixture, true);
    let mut host = start(&fixture);
    let session = create(&host, &fixture).unwrap();
    let mut connection = Connection::open(&fixture).unwrap();
    let trace = fixture_trace(false, false);
    let hello = trace["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["frame"]["frame"] == "hello")
        .unwrap();
    connection.send(&hello["frame"]).unwrap();
    assert_eq!(connection.control().unwrap()["frame"], "welcome");
    let node = review_rows(&host)
        .into_iter()
        .find(|row| row.session == session)
        .unwrap()
        .node;
    connection
        .send(&json!({"frame":"request","id":1,"op":{"op":"attach","node":node}}))
        .unwrap();
    let attached = connection.control().unwrap();
    let attachment = attached["outcome"]["ok"]["attachment"].clone();
    assert!(attachment.is_u64(), "{attached}");
    // Attachment alone need not produce chrome. One real refresh warms its
    // asynchronous reader and settles initial projection before the audit.
    connection.send(&json!({"frame":"request","id":2,"op":{"op":"intent","attachment":attachment,"blobs":[],"intent":{"kind":"extension","body":"refresh_composer_chrome"}}})).unwrap();
    let (mut acknowledged, mut refreshed) = (false, false);
    while !acknowledged || !refreshed {
        let frame = connection.read().unwrap();
        acknowledged |= frame["frame"] == "response" && frame["id"] == 2;
        refreshed |= is_chrome(&frame);
    }
    for audit in audits() {
        for pair in audit["pairs"].as_array().unwrap() {
            let mut request = pair["request"].clone();
            if pair["read"] == "list_agent_acknowledgements" {
                request["op"]["intent"]["body"]["list_agent_acknowledgements"]["tree_id"] =
                    json!(node.tree_id);
            }
            let _: ClientFrame<plak::wire_transport::PlakExtension> =
                serde_json::from_value(request.clone()).unwrap();
            request["op"]["attachment"] = attachment.clone();
            let before = snapshot(&fixture);
            connection.send(&request).unwrap();
            let (mut acknowledged, mut refreshed) = (false, false);
            while !acknowledged || !refreshed {
                let frame = connection.read().unwrap();
                if frame["frame"] == "response" {
                    assert_eq!(frame, pair["acknowledgement"]);
                    acknowledged = true;
                }
                refreshed |= match pair["read"].as_str().unwrap() {
                    "refresh_composer_chrome" => is_chrome(&frame),
                    "resync_transcript" => frame["frame"] == "snapshot" && frame["last"] == true,
                    "list_agent_acknowledgements" => frame["observation"]["body"]
                        .get("agent_acknowledgements")
                        .is_some(),
                    other => panic!("unreviewed legal read: {other}"),
                };
            }
            assert_eq!(
                snapshot(&fixture),
                before,
                "{}: {request}",
                audit["fixture"]
            );
            assert_eq!(dispatches(&fixture), 0);
        }
    }
    drop(connection.writer.take());
    connection.closed(ReplayMode::Journey).unwrap();
    stop(&mut host);
}
