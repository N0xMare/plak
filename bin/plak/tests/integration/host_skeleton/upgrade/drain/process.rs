//! A foreground wait becomes a joined promoted job before the same-triple transfer.

use super::super::super::runner::{alive, job};
use super::*;

#[test]
fn host_step_drain_notice_promotes_a_foreground_process_before_same_triple_handoff() {
    process_handoff("0", ProcessWait::Foreground);
}

#[test]
fn host_step_drain_notice_yields_job_output_under_finite_and_idle_budgets() {
    for mode in ["0", "idle"] {
        process_handoff(mode, ProcessWait::Output);
    }
}

#[derive(Clone, Copy)]
enum ProcessWait {
    Foreground,
    Output,
}

fn process_handoff(mode: &str, waiting: ProcessWait) {
    let Some(fixture) = fixture() else { return };
    let (mut host, _client, session, node, pid) = foreground(&fixture, &[], waiting);
    let _cleanup = HomeHost(fixture.home());
    let before = store(&fixture)
        .load(&session)
        .unwrap()
        .plakous_locator
        .unwrap();
    // The replacement has the same tool contract but answers its continuation,
    // rather than issuing the source's process call again.
    fixture.write_script(
        &json!({"version":1,"process_tools":true,"steps":[{"output":{"text":ANSWER}}]}),
    );
    let id = binary(&fixture);
    let result = output_env(
        &fixture,
        &["host", "upgrade", "--expect-binary", &id, "--drain", mode],
        false,
        &[("PLAK_TEST_FORCE_HOST_UPGRADE", "1")],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let deadline = Instant::now() + WAIT;
    loop {
        let views = store(&fixture).load_views(&session).unwrap().unwrap();
        if views.state.counters.root_turns == 2 && views.state.open_turns.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "continuation did not finish under {mode}: counters={:?} open={} decisions={} dispatches={} standing={:?}",
            views.state.counters,
            views.state.open_turns.len(),
            root_record(&fixture, &session).1,
            dispatches(&fixture),
            plakous_hosted::runtime::root_standing(&store(&fixture), &session)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        alive(pid),
        "promotion must survive transfer, not merely acknowledge control"
    );
    assert_eq!(
        dispatches(&fixture),
        if matches!(waiting, ProcessWait::Output) {
            3
        } else {
            2
        }
    );
    assert_eq!(job(&fixture).0, pid);
    assert_eq!(root_record(&fixture, &session).1, 1);
    let after = store(&fixture)
        .load(&session)
        .unwrap()
        .plakous_locator
        .unwrap();
    assert_eq!(before.session_id(), after.session_id());
    assert_eq!(before.branch_id(), after.branch_id());
    let mut client = Client::connect_at(&host.socket);
    client.attach(10, &node);
    assert_eq!(
        client.request(11, Op::StopSession { session }),
        Outcome::Ok(Answer::Stopped {
            runner_pending: false
        })
    );
    while alive(pid) {
        assert!(
            Instant::now() < deadline,
            "the explicit stop did not stop the promoted job"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    stop(&mut host);
}

fn foreground(
    fixture: &Fixture,
    env: &[(&str, String)],
    waiting: ProcessWait,
) -> (Host, Client, String, NodeAddress, i32) {
    fixture.write_script(&process_script_for(fixture, waiting));
    let host = start_env(fixture, &[], env);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created")
    };
    let attachment = client.attach(2, &node);
    client.retain_observations();
    prompt(&mut client, attachment, "keep the foreground process alive");
    let (pid, _) = job(fixture);
    assert!(alive(pid));
    if matches!(waiting, ProcessWait::Output) {
        let deadline = Instant::now() + WAIT;
        client.read_deadline = Some(deadline);
        observations::tool_row(&mut client, "process_session_poll", false);
    }
    assert_eq!(
        dispatches(fixture),
        if matches!(waiting, ProcessWait::Output) {
            2
        } else {
            1
        }
    );
    // Subsequent waits use process/store evidence, not transcript events.
    assert_eq!(
        client.request(4, Op::Detach { attachment }),
        Outcome::Ok(Answer::Detached)
    );
    (host, client, session, node, pid)
}

fn process_script_for(fixture: &Fixture, waiting: ProcessWait) -> serde_json::Value {
    let marker = fixture.root.join("local-job.marker");
    let quoted = marker.display().to_string().replace('\'', "'\\''");
    let mut script = json!({"version":1,"process_tools":true,"steps":[
        {"output":{"tool":{"name":"process_session_start","arguments":{
            "command":format!("printf '%s %s\\n' \"$$\" \"$PPID\" >> '{quoted}'; read line"),
            "workdir":".","login":false,"tty":false,"keep_stdin_open":true,
            "yield_time_ms":600_000,"max_output_bytes":4096
        }}},"finish":"tool_calls"},
        {"output":{"text":"this source round must remain behind the step gate"}}
    ]});
    if matches!(waiting, ProcessWait::Output) {
        script["steps"][0]["output"]["tool"]["arguments"]["yield_time_ms"] = json!(0);
        script["steps"].as_array_mut().unwrap().insert(1, json!({"output":{"tool":{"name":"process_session_poll","arguments":{"session_id":"$session_id","cursor":"$cursor","yield_time_ms":300_000,"max_output_bytes":4096,"max_output_records":64}}},"finish":"tool_calls"}));
    }
    script
}

#[test]
fn host_step_drain_changed_triple_rechecks_a_process_started_during_preparation() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    fixture.write_script(&process_script_for(&fixture, ProcessWait::Foreground));
    let release = fixture.root.join("provider-release");
    let drain_release = fixture.root.join("drain-release");
    let mut host = start_env(
        &fixture,
        &[],
        &[
            (
                "PLAK_TEST_HOST_UPGRADE_RUNNER_REVISION",
                "synthetic-destination".into(),
            ),
            ("PLAK_TEST_PROVIDER_RELEASE", release.display().to_string()),
            (
                "PLAK_TEST_HOST_UPGRADE_DRAIN_GATE",
                drain_release.display().to_string(),
            ),
        ],
    );
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, fresh_session(&fixture))
    else {
        panic!("not created")
    };
    let attachment = client.attach(2, &node);
    prompt(
        &mut client,
        attachment,
        "start a process after the first retention check",
    );
    wait_for_provider_dispatch(&fixture);
    let mut requester = Requester(Some(
        fixture
            .command()
            .args([
                "host",
                "upgrade",
                "--expect-binary",
                &binary(&fixture),
                "--drain",
                "5",
            ])
            .env("PLAK_TEST_FORCE_HOST_UPGRADE", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    let deadline = Instant::now() + WAIT;
    loop {
        let probe = new_session(&fixture.root.join("absent-workspace"));
        if let Outcome::Refused(reason) = client.request(10, probe)
            && format!("{reason:?}").contains("upgrad")
        {
            break;
        }
        assert!(Instant::now() < deadline, "upgrade never paused admissions");
        assert!(requester.0.as_mut().unwrap().try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(10));
    }
    std::fs::write(release, "continue").unwrap();
    let (pid, _) = job(&fixture);
    assert_eq!(
        root_record(&fixture, &session).1,
        0,
        "process must start under the source, before its hold"
    );
    std::fs::write(drain_release, "continue").unwrap();
    while requester.0.as_mut().unwrap().try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "late process did not refuse surrender"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let result = requester.0.take().unwrap().wait_with_output().unwrap();
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr)
            .contains("host_upgrade_process_retention_unavailable"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(alive(pid), "refusal must retain the process on the source");
    assert_eq!(root_record(&fixture, &session).1, 0);
    assert!(matches!(
        client.request(11, fresh_session(&fixture)),
        Outcome::Ok(Answer::Created { .. })
    ));
    assert!(!fixture.home().join("host/upgrades").exists());
    assert_eq!(
        client.request(12, Op::StopSession { session }),
        Outcome::Ok(Answer::Stopped {
            runner_pending: false
        })
    );
    stop(&mut host);
}

#[test]
fn host_step_drain_changed_triple_with_a_foreground_process_refuses_before_pause() {
    changed_triple_refusal(ProcessWait::Foreground);
}

#[test]
fn host_step_drain_changed_triple_with_a_background_job_refuses_before_pause() {
    changed_triple_refusal(ProcessWait::Output);
}

fn changed_triple_refusal(waiting: ProcessWait) {
    let Some(fixture) = fixture() else { return };
    let (mut host, mut client, session, node, pid) = foreground(
        &fixture,
        &[(
            "PLAK_TEST_HOST_UPGRADE_RUNNER_REVISION",
            "synthetic-destination".to_owned(),
        )],
        waiting,
    );
    let _cleanup = HomeHost(fixture.home());
    let result = output_env(
        &fixture,
        &[
            "host",
            "upgrade",
            "--expect-binary",
            &binary(&fixture),
            "--drain",
            "0",
        ],
        false,
        &[("PLAK_TEST_FORCE_HOST_UPGRADE", "1")],
    );
    assert!(!result.status.success());
    let message = String::from_utf8_lossy(&result.stderr);
    assert!(
        message.contains("host_upgrade_process_retention_unavailable"),
        "{message}"
    );
    assert!(
        !message.contains("waiting "),
        "refusal must precede the admission pause"
    );
    assert!(alive(pid));
    assert_eq!(
        dispatches(&fixture),
        if matches!(waiting, ProcessWait::Output) {
            2
        } else {
            1
        }
    );
    assert_eq!(root_record(&fixture, &session).1, 0);
    client.attach(10, &node);
    assert_eq!(
        client.request(11, Op::StopSession { session }),
        Outcome::Ok(Answer::Stopped {
            runner_pending: false
        })
    );
    assert_eq!(client.request(12, Op::Ping), Outcome::Ok(Answer::Pong));
    assert!(!fixture.home().join("host/upgrades").exists());
    stop(&mut host);
}
