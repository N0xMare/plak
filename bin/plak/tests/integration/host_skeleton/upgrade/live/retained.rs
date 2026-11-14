use super::*;

#[test]
fn host_move_live_terminal_during_surrender_is_durable_before_group_ready() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let (mut host, session, _) = runner::start_job_env(
        &fixture,
        false,
        &[
            (ENABLED, "1".into()),
            ("PLAK_TEST_HOST_UPGRADE_CUT", "move-after-commit".into()),
        ],
    );
    let pid = runner::job(&fixture).0;
    fixture.write_script(
        &json!({"version":1,"process_tools":true,"steps":[{"output":{"text":ANSWER}}]}),
    );
    let result = output_env(
        &fixture,
        &["host", "upgrade", "--expect-binary", &binary(&fixture)],
        true,
        &[(ENABLED, "1")],
    );
    assert!(!result.status.success());
    assert_eq!(receipts(&fixture)[0]["phase"], "committed");
    assert_eq!(host.child.take().unwrap().wait().unwrap().code(), Some(86));
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let deadline = Instant::now() + WAIT;
    while runner::alive(pid) {
        assert!(
            Instant::now() < deadline,
            "fixture job did not join its terminal"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut target = start_env(
        &fixture,
        &[],
        &[
            (ENABLED, "1".into()),
            ("PLAK_TEST_STATE_KEY", DESTINATION.into()),
        ],
    );
    wait_group(&fixture, &session, "ready");
    let locator = dormant::identity(&fixture, DESTINATION, &session).0;
    let snapshot = plakous::sessions::SessionBackend::inspect_move_source_cut_sqlite(
        fixture
            .home()
            .join("state")
            .join(DESTINATION)
            .join("runtime.sqlite3"),
        &[locator.session_id()],
        plakous::sessions::host_move::MoveJobCut::Retained,
    )
    .unwrap();
    let terminal_count = snapshot
        .lines()
        .iter()
        .map(|line| line.terminals.len())
        .sum::<usize>();
    assert_eq!(
        terminal_count, 1,
        "the final sweep did not preserve its terminal before Ready"
    );
    assert_eq!(
        drain::dispatches(&fixture),
        2,
        "terminal reconciliation dispatched another model call"
    );
    stop(&mut target);
}

#[test]
fn host_move_live_promoted_job_keeps_its_allocation_through_group_ready() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let (mut host, session, node) =
        runner::start_job_env(&fixture, false, &[(ENABLED, "1".into())]);
    let before = dormant::identity(&fixture, &plak::home::state::state_key(), &session).0;
    let (pid, _) = runner::job(&fixture);
    fixture.write_script(
        &json!({"version":1,"process_tools":true,"steps":[{"output":{"text":ANSWER}}]}),
    );
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
        true,
        &[(ENABLED, "1")],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let receipt = receipts(&fixture).remove(0);
    assert_eq!(receipt["phase"], "acknowledged");
    assert_eq!(receipt["groups"][0]["phase"], "ready");
    let after = dormant::identity(&fixture, DESTINATION, &session).0;
    assert_eq!(before.session_id(), after.session_id());
    assert_eq!(before.branch_id(), after.branch_id());
    assert_eq!(runner::job(&fixture).0, pid);
    assert!(runner::alive(pid));
    let mut client =
        Client::connect_at(&fixture.home().join(format!("host/run/{DESTINATION}.sock")));
    client.attach(70, &node);
    assert_eq!(
        client.request(71, Op::StopSession { session }),
        Outcome::Ok(Answer::Stopped {
            runner_pending: false
        })
    );
    let deadline = Instant::now() + WAIT;
    while runner::alive(pid) {
        assert!(
            Instant::now() < deadline,
            "destination did not control the original allocation"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    stop(&mut host);
}

#[test]
fn host_move_live_offline_runner_withholds_only_its_dependency_group() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let (daemon, path, id) = runner::remote_fixture(&fixture);
    let remote = remote::seed(&fixture, &path, &id, 91);
    let local_workspace = fixture.root.join("independent-workspace");
    std::fs::create_dir(&local_workspace).unwrap();
    let (mut host, local, _) = runner::start_job_at(
        &fixture,
        false,
        &[
            (ENABLED, "1".into()),
            ("PATH", path.clone()),
            ("PLAK_TEST_HOST_UPGRADE_CUT", "move-after-commit".into()),
        ],
        &local_workspace,
    );
    runner::remote_ready(&mut Client::connect(&host), &id);
    let pid = runner::job(&fixture).0;
    fixture.write_script(
        &json!({"version":1,"process_tools":true,"steps":[{"output":{"text":ANSWER}}]}),
    );
    let result = output_env(
        &fixture,
        &["host", "upgrade", "--expect-binary", &binary(&fixture)],
        true,
        &[(ENABLED, "1"), ("PATH", &path)],
    );
    assert!(!result.status.success());
    assert_eq!(receipts(&fixture)[0]["phase"], "committed");
    assert_eq!(host.child.take().unwrap().wait().unwrap().code(), Some(86));
    daemon.kill();
    let ssh = fixture.root.join("ssh-bin/ssh");
    let original = std::fs::read_to_string(&ssh).unwrap();
    std::fs::write(&ssh, "#!/bin/sh\nexit 1\n").unwrap();
    let mut target = start_env(
        &fixture,
        &[],
        &[
            (ENABLED, "1".into()),
            ("PATH", path.clone()),
            ("PLAK_TEST_STATE_KEY", DESTINATION.into()),
        ],
    );
    runner::wait_local_runner_ready(&target);
    wait_group(&fixture, &local, "ready");
    let authority = receipts(&fixture).remove(0);
    assert_eq!(authority["phase"], "committed");
    assert_ne!(group(&authority, &remote)["phase"], "ready");
    assert!(runner::alive(pid));
    let refused = output_env(
        &fixture,
        &["run", "--session", &remote, "--", "never"],
        true,
        &[(ENABLED, "1"), ("PATH", &path)],
    );
    assert!(!refused.status.success());
    std::fs::write(ssh, original).unwrap();
    daemon.start();
    wait_group(&fixture, &remote, "ready");
    assert_eq!(receipts(&fixture)[0]["phase"], "acknowledged");
    assert_eq!(runner::job(&fixture).0, pid);
    stop(&mut target);
}

fn group<'a>(receipt: &'a serde_json::Value, session: &str) -> &'a serde_json::Value {
    receipt["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| {
            group["products"]
                .as_array()
                .unwrap()
                .contains(&json!(session))
        })
        .unwrap()
}

fn wait_group(fixture: &Fixture, session: &str, phase: &str) {
    let deadline = Instant::now() + WAIT;
    loop {
        let authority = receipts(fixture).remove(0);
        if group(&authority, session)["phase"] == phase {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "group did not reach {phase}: {authority}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
