use super::*;
use serde_json::Value;

#[test]
fn served_child_previews_do_not_create_gaps_for_either_viewer() {
    let Some((fixture, _)) = child_fixture(true) else {
        return;
    };
    let release = fixture.root.join("release-preview");
    let finish = fixture.root.join("finish-child");
    std::fs::write(
        fixture.root.join("child.json"),
        json!({
            "version":1,"steps":[{
                "stream":[{"gate":{"file":release}},{"reasoning":"child reasoning"},
                          {"text":"child assistant preview"}],
                "output":{"tool":{"name":"bash","arguments":{"command":"printf child"}}},
                "finish":"tool_calls"
            },{
                "stream":[{"gate":{"file":finish}}],
                "output":{"tool":{"name":"submit_result","arguments":{
                    "turn_token":1,"output_json":"{\"summary\":\"review complete\"}"
                }}},"finish":"tool_calls"
            }]
        })
        .to_string(),
    )
    .unwrap();

    let mut host = start(&fixture);
    let mut root = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        root.request(1, new_session(&fixture.workspace()))
    else {
        panic!("session not created")
    };
    let root_id = root.attach(2, &node);
    root.prompt(3, root_id, "delegate and idle", ANSWER);
    idle_turns(&fixture, &session, 1);

    let child_node = NodeAddress {
        agent_id: "c1".into(),
        ..node.clone()
    };
    let scope = json!({"agent":child_node.agent_id});
    let mut child = Client::connect(&host);
    // Give this link a real root history binding before switching it to a child.
    child.attach(1, &node);
    let child_id = child.attach(2, &child_node);
    root.send(&ClientFrame::Request {
        id: 4,
        op: Op::Intent {
            attachment: root_id,
            client_id: None,
            intent: SessionIntent::OpenAgentTranscript {
                agent_id: child_node.agent_id,
            },
            blobs: Vec::new(),
        },
    });
    root.read_deadline = Some(Instant::now() + WAIT);
    child.read_deadline = root.read_deadline;
    wait_for_scope(&mut root, root_id, &scope);
    wait_for_scope(&mut child, child_id, &scope);

    raw_request(
        &mut child,
        5,
        json!({"op":"older_history","attachment":child_id,"cursor":"root-history"}),
    );
    loop {
        let frame = received(&mut child);
        if frame["frame"] == "response" && frame["id"] == 5 {
            assert_eq!(
                frame["outcome"]["refused"]["refusal"], "cursor_expired",
                "{frame}"
            );
            break;
        }
    }

    let viewers = [(root, root_id), (child, child_id)];
    let mut viewers = viewers.map(|(mut client, attachment)| {
        ask_status(&mut client, attachment, 6);
        let status = read_status(&mut client, attachment, 6, |_| {});
        (client, attachment, status)
    });
    std::fs::write(release, []).unwrap();
    for (client, attachment, before) in &mut viewers {
        let mut sequence = None;
        let mut numbered = 0;
        let mut observe = |frame: &Value| {
            assert_ne!(frame["frame"], "snapshot", "unexpected baseline: {frame}");
            if frame["frame"] == "event" {
                assert_eq!(frame["attachment"], *attachment, "{frame}");
                let seq = frame["seq"].as_u64().unwrap();
                if let Some(last) = sequence.replace(seq) {
                    assert_eq!(seq, last + 1, "{frame}");
                }
                numbered += 1;
            }
        };
        loop {
            let frame = received(client);
            observe(&frame);
            let event = &frame["observation"];
            if event["kind"] == "transcript_tool_stream"
                && event["body"]["scope"] == scope
                && event["body"]["tool"]["name"] == "bash"
                && event["body"]["tool"]["state"] == "done"
            {
                break;
            }
        }
        ask_status(client, *attachment, 7);
        let after = read_status(client, *attachment, 7, observe);
        assert!(numbered >= 2, "need more than one numbered event");
        assert_eq!(
            after, *before,
            "a hidden preview affected attachment counters"
        );
    }
    std::fs::write(finish, []).unwrap();
    submitted_result(&fixture, &node);
    drop(viewers);
    stop(&mut host);
}

fn wait_for_scope(client: &mut Client, attachment: u64, scope: &Value) {
    loop {
        let frame = received(client);
        if frame["frame"] == "snapshot"
            && frame["attachment"] == attachment
            && frame["scope"] == *scope
            && frame["last"] == true
        {
            return;
        }
    }
}

fn received(client: &mut Client) -> Value {
    let frame: Value = serde_json::from_str(&client.read_wire_line()).unwrap();
    assert_ne!(frame["frame"], "resync_required", "{frame}");
    frame
}

fn raw_request(client: &mut Client, id: u64, op: Value) {
    writeln!(
        client.stream,
        "{}",
        json!({"frame":"request","id":id,"op":op})
    )
    .unwrap();
}

fn ask_status(client: &mut Client, attachment: u64, id: u64) {
    raw_request(
        client,
        id,
        json!({"op":"attachment_status","attachment":attachment}),
    );
}

fn read_status(
    client: &mut Client,
    attachment: u64,
    id: u64,
    mut observe: impl FnMut(&Value),
) -> (u64, u64) {
    loop {
        let frame = received(client);
        if frame["frame"] == "attachment_status" && frame["request_id"] == id {
            assert_eq!(frame["attachment"], attachment);
            return (
                frame["seq_gaps"].as_u64().unwrap(),
                frame["deltas_evicted"].as_u64().unwrap(),
            );
        }
        observe(&frame);
    }
}
