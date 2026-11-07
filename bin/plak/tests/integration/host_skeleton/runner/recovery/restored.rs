//! An admitted tool decision is restored, not resubmitted.

use super::*;

fn wait_for_approval(client: &mut Client, require_restored: bool) {
    client
        .stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut restored = !require_restored;
    let mut approval = false;
    loop {
        let mut line = String::new();
        assert_ne!(client.reader.read_line(&mut line).unwrap(), 0);
        let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_ne!(frame["frame"], "detached", "{line}");
        restored |= frame["observation"]["body"]["detail"]["restoring_admitted_turn"].is_object();
        approval |= frame["observation"]["kind"] == "approval_required";
        if restored && approval {
            break;
        }
    }
}

#[test]
fn a_sigterm_during_a_restored_admitted_turn_records_its_own_handoff() {
    let Some(fixture) = fixture() else { return };
    fixture.write_script(&json!({"version": 1, "tool_policy": "decision_required", "steps": [
        {"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls"},
        {"output": {"text": ANSWER}, "finish": "stop"}
    ]}));
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created")
    };
    let attachment = client.attach(2, &node);
    client.send(&ClientFrame::Request {
        id: 3,
        op: Op::Intent {
            attachment,
            intent: SessionIntent::Prompt(Draft::from("admitted turn")),
            blobs: Vec::new(),
        },
    });
    wait_for_approval(&mut client, false);
    kill(&mut host);
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    client.send(&ClientFrame::Request {
        id: 1,
        op: Op::Attach { node },
    });
    wait_for_approval(&mut client, true);
    stop(&mut host);
    assert_eq!(
        root_record(&fixture, &session)
            .0
            .iter()
            .filter(|cause| **cause == RootInterruptCause::Handoff)
            .count(),
        1
    );
}
