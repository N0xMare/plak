//! Real product-pack child and inbox awaits yield before the child completes.

use super::*;
use plakous_host_protocol::SessionObservation;

#[test]
fn host_step_drain_notice_yields_named_child_wait_under_finite_and_idle_budgets() {
    for mode in ["0", "idle"] {
        observation(mode, Some(json!(["$result.agent_id"])));
    }
}

#[test]
fn host_step_drain_notice_yields_pending_mail_wait_under_finite_and_idle_budgets() {
    for mode in ["0", "idle"] {
        observation(mode, None);
    }
}

pub(super) fn tool_row(client: &mut Client, name: &str, finished: bool) {
    loop {
        let HostFrame::Event { observation, .. } = client.next() else {
            continue;
        };
        let rows = match observation {
            SessionObservation::Row(row) => vec![row],
            SessionObservation::TranscriptPatch(patch) => {
                patch.upserts.into_iter().map(|item| item.row).collect()
            }
            SessionObservation::PresentationSnapshot(snapshot) => {
                snapshot.items.into_iter().map(|item| item.row).collect()
            }
            _ => Vec::new(),
        };
        for row in rows {
            if let Some(tool) = row.as_tool()
                && (tool.name == name
                    || tool.exposed_name.as_deref() == Some(name)
                    || (name == "wait" && tool.name == "wait_agent"))
            {
                if !finished && tool.state == "running" {
                    return;
                }
                if finished && (tool.state.starts_with("done") || tool.state == "succeeded") {
                    assert!(
                        tool.wait
                            .as_ref()
                            .and_then(|wait| wait.woke_by.as_deref())
                            .is_some_and(|wake| wake.contains("notice")),
                        "wait did not yield to Notice: {:?}",
                        tool.wait
                    );
                    return;
                }
            }
        }
    }
}

fn observation(mode: &str, agents: Option<serde_json::Value>) {
    let Some(fixture) = fixture() else { return };
    let child_script = fixture.root.join("child-script.json");
    let child_release = fixture.root.join("child-release");
    std::fs::write(&child_script, serde_json::to_vec(&json!({"version":1,"steps":[{"stream":[{"gate":{"file":child_release}}],"output":{"text":"child completed naturally"}}]})).unwrap()).unwrap();
    let mut script = spawning_script();
    script["product_child_script"] = json!(child_script);
    // The real child binder uses the product API pack: its wait_agent slot
    // is presented as wait, rather than the standalone scripted vocabulary.
    script["steps"][1]["output"]["tool"]["name"] = json!("wait");
    script["steps"][1]["output"]["tool"]["arguments"] = json!({"timeout_ms":300_000});
    if let Some(agents) = agents {
        script["steps"][1]["output"]["tool"]["arguments"]["ids"] = agents;
    }
    fixture.write_script(&script);
    let mut host = start(&fixture);
    let _cleanup = HomeHost(fixture.home());
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, fresh_session(&fixture))
    else {
        panic!("not created")
    };
    let attachment = client.attach(2, &node);
    client.retain_observations();
    prompt(
        &mut client,
        attachment,
        "wait for delegated work or inbox mail",
    );
    let deadline = Instant::now() + WAIT;
    client.read_deadline = Some(deadline);
    tool_row(&mut client, "wait", false);
    // Reopened bindings answer continuations; the source's already-dispatched
    // child generation remains gated until the explicit release below.
    script["steps"] = json!([{"output":{"text":ANSWER}}]);
    fixture.write_script(&script);
    std::fs::write(
        &child_script,
        serde_json::to_vec(&json!({"version":1,"steps":[{"output":{"text":"continued child"}}]}))
            .unwrap(),
    )
    .unwrap();
    let id = binary(&fixture);
    let mut command = fixture.command();
    command
        .args(["host", "upgrade", "--expect-binary", &id, "--drain", mode])
        .env("PLAK_TEST_FORCE_HOST_UPGRADE", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut requester = Requester(Some(command.spawn().unwrap()));
    if mode == "idle" {
        tool_row(&mut client, "wait", true);
        assert!(
            requester.0.as_mut().unwrap().try_wait().unwrap().is_none(),
            "idle must still wait for the child's model generation"
        );
        assert!(!child_release.exists());
        std::fs::write(&child_release, "continue").unwrap();
    }
    while requester.0.as_mut().unwrap().try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "observation held the upgrade behind its 300s timeout"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let result = requester.0.take().unwrap().wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let mut client = Client::connect_at(&host.socket);
    client.attach(10, &node);
    let deadline = Instant::now() + WAIT;
    while root_record(&fixture, &session).1 == 0 {
        assert!(
            Instant::now() < deadline,
            "no once-only continuation after reconnect: mode={mode} standing={:?} counters={:?}",
            plakous_hosted::runtime::root_standing(&store(&fixture), &session),
            store(&fixture)
                .load_views(&session)
                .unwrap()
                .unwrap()
                .state
                .counters
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        client.request(
            11,
            Op::StopSession {
                session: session.clone()
            }
        ),
        Outcome::Ok(Answer::Stopped {
            runner_pending: false
        })
    );
    assert_eq!(root_record(&fixture, &session).1, 1);
    stop(&mut host);
}
