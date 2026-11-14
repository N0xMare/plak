//! A resumed root consumes child results in the same turn that Notice yields.

use super::*;

#[test]
fn a_continuing_root_receives_a_child_notice_before_it_idles() {
    let Some((fixture, script)) = child_fixture(true) else {
        return;
    };
    let continuation = json!([
        {"output":{"tool":{"name":"process_session_start","arguments":{
            "command":"read line","workdir":".","login":false,"tty":false,
            "keep_stdin_open":true,"yield_time_ms":600_000,"max_output_bytes":4096
        }}},"finish":"tool_calls"},
        {"output":{"tool":{"name":"wait","arguments":{"ids":["c1"],"timeout_ms":0}}},"finish":"tool_calls"},
        {"output":{"text":ANSWER}}
    ]);
    let (mut host, mut client, session, node, _) = handed_off_root(&fixture, script, continuation);
    wait_for_process(&mut client);
    std::fs::write(fixture.root.join("release-child"), "continue").unwrap();
    submitted_result(&fixture, &node);
    idle_turns(&fixture, &session, 2);
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let records = plak::store::session_records::SqliteSessionStore::new(paths.clone());
    let record = records.load(&session).unwrap();
    assert!(
        record
            .transcript
            .iter()
            .filter_map(|row| row.as_tool())
            .any(|tool| {
                tool.name == "process_session_start"
                    && tool.detail.as_deref().is_some_and(|detail| {
                        detail.contains("New messages are waiting; end this turn to receive them.")
                    })
            }),
        "the quiet process must yield for Notice, not finish or time out"
    );
    let trees = SqliteTreeStore::open(&paths.subagents_database()).unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        let tree = ProductAgentTree::reopen(trees.load(&node.tree_id).unwrap().unwrap());
        if tree.unread_mail(&tree.root_agent_id).count() == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "wait did not consume the result in turn 2"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    idle_stays(&fixture, &session, 2);
    drop(client);
    stop(&mut host);
}

fn wait_for_process(client: &mut Client) {
    client.read_deadline = Some(Instant::now() + WAIT);
    loop {
        let HostFrame::Event { observation, .. } = client.next() else {
            continue;
        };
        let rows = match observation {
            plakous_host_protocol::SessionObservation::Row(row) => vec![row],
            plakous_host_protocol::SessionObservation::TranscriptPatch(patch) => {
                patch.upserts.into_iter().map(|item| item.row).collect()
            }
            plakous_host_protocol::SessionObservation::PresentationSnapshot(snapshot) => {
                snapshot.items.into_iter().map(|item| item.row).collect()
            }
            _ => Vec::new(),
        };
        if rows
            .iter()
            .filter_map(|row| row.as_tool())
            .any(|tool| tool.name == "process_session_start" && tool.state == "running")
        {
            return;
        }
    }
}
