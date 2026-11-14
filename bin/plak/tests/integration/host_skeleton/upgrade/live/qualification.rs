//! Real host deaths leave live cuts reversible before surrender and forward-only after it.
use super::*;

mod children;

#[test]
fn host_move_live_offline_dependency_stays_withheld_across_destination_restarts() {
    retained::offline_runner(true);
}

#[test]
fn host_move_live_predecision_drain_cuts_keep_the_source_openable() {
    for cut in [
        "move-after-live-inspection",
        "move-after-live-gates",
        "move-after-live-handoff",
    ] {
        let Some(fixture) = fixture() else { return };
        let _cleanup = HomeHost(fixture.home());
        let configuration = fixture.home().join("config.toml");
        let original = std::fs::read_to_string(&configuration).unwrap();
        std::fs::write(
            configuration,
            format!("{original}\n[host]\nresume_after_loss = \"hold\"\n"),
        )
        .unwrap();
        let (mut host, _, session, _, release) = drain::busy_with(
            &fixture,
            &[
                (ENABLED, "1".into()),
                ("PLAK_TEST_HOST_UPGRADE_CUT", cut.into()),
            ],
        );
        runner::wait_local_runner_ready(&host);
        let before = dormant::identity(&fixture, &plak::home::state::state_key(), &session);
        let result = upgrade(&fixture);
        assert!(!result.status.success(), "{cut} did not fire");
        assert_eq!(
            runner::cut_exit(host.child.as_mut().unwrap()).code(),
            Some(86)
        );
        host.child = None;
        assert!(!release.exists());
        assert!(
            !fixture
                .home()
                .join("state")
                .join(DESTINATION)
                .join("runtime.sqlite3")
                .exists()
        );
        if fixture.home().join("host/upgrades").exists() {
            assert_eq!(receipts(&fixture), Vec::<serde_json::Value>::new());
        }
        let mut restored = start_env(&fixture, &[], &[(ENABLED, "1".into())]);
        assert!(
            Client::connect(&restored)
                .rows(40)
                .iter()
                .any(|row| row.session == session)
        );
        let after = dormant::identity(&fixture, &plak::home::state::state_key(), &session);
        assert_eq!(after.0.session_id(), before.0.session_id());
        assert_eq!(after.0.branch_id(), before.0.branch_id());
        assert_eq!(after.1, before.1);
        stop(&mut restored);
    }
}

#[test]
fn host_move_live_retained_job_recovers_owner_and_ready_crashes_without_restarting_work() {
    for cut in [
        "move-after-owner-witnesses",
        "move-after-lease-only-heads",
        "move-before-transfer",
        "move-after-transfer",
        "move-after-all-transfers",
        "move-before-ready",
        "move-after-ready",
    ] {
        retained_cut(cut, false);
    }
}

#[test]
fn host_move_live_concurrent_terminal_recording_is_once_across_crashes() {
    for cut in [
        "move-before-job-terminals",
        "move-after-job-terminals",
        "move-before-ready",
        "move-after-ready",
    ] {
        retained_cut(cut, true);
    }
}

fn retained_cut(cut: &str, terminal: bool) {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let (mut source, session, node) = runner::start_job_env(
        &fixture,
        false,
        &[
            (ENABLED, "1".into()),
            ("PLAK_TEST_HOST_UPGRADE_CUT", "move-after-commit".into()),
        ],
    );
    let allocation = allocation(&fixture);
    let pid = runner::job(&fixture).0;
    let before = dormant::identity(&fixture, &plak::home::state::state_key(), &session);
    fixture.write_script(
        &json!({"version":1,"process_tools":true,"steps":[{"output":{"text":ANSWER}}]}),
    );
    commit_source(&fixture, &mut source);
    if terminal {
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        )
        .unwrap();
        stopped(pid);
    }
    crash_destination(&fixture, cut);
    let authority = receipts(&fixture).remove(0);
    assert_eq!(
        authority["groups"][0]["phase"],
        if cut == "move-after-ready" {
            "ready"
        } else {
            "published"
        }
    );
    assert_eq!(
        native_count(
            &fixture,
            "SELECT count(*) FROM imported_sessions WHERE execution_open<>0"
        ),
        0
    );
    if terminal {
        assert_eq!(
            native_count(&fixture, "SELECT count(*) FROM line_job_terminals"),
            i64::from(cut != "move-before-job-terminals")
        );
    } else {
        assert!(runner::alive(pid));
        assert_eq!(self::allocation(&fixture), allocation);
    }
    for _ in 0..2 {
        let mut target = destination(&fixture);
        wait_ready(&fixture, &session, &target, cut);
        assert_eq!(receipts(&fixture)[0]["phase"], "acknowledged");
        let after = dormant::identity(&fixture, DESTINATION, &session);
        assert_eq!(after.0.session_id(), before.0.session_id());
        assert_eq!(after.0.branch_id(), before.0.branch_id());
        assert_eq!(after.1, before.1);
        assert_eq!(
            drain::dispatches(&fixture),
            2,
            "recovery replayed the source process call: {cut}"
        );
        if terminal {
            assert_eq!(
                native_count(&fixture, "SELECT count(*) FROM line_job_terminals"),
                1
            );
        } else {
            assert!(runner::alive(pid));
            assert_eq!(self::allocation(&fixture), allocation);
        }
        stop(&mut target);
    }
    if !terminal {
        let mut target = destination(&fixture);
        let mut client = Client::connect(&target);
        client.attach(80, &node);
        assert_eq!(
            client.request(81, Op::StopSession { session }),
            Outcome::Ok(Answer::Stopped {
                runner_pending: false
            })
        );
        stopped(pid);
        stop(&mut target);
    }
}

fn wait_ready(fixture: &Fixture, session: &str, target: &Host, cut: &str) {
    let deadline = Instant::now() + WAIT;
    loop {
        let receipt = receipts(fixture).remove(0);
        if receipt["groups"].as_array().unwrap().iter().any(|group| {
            group["products"]
                .as_array()
                .unwrap()
                .contains(&json!(session))
                && group["phase"] == "ready"
        }) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{cut}: group not Ready: {receipt}; messages={:?}",
            target.lines.try_iter().collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn upgrade(fixture: &Fixture) -> std::process::Output {
    output_env(
        fixture,
        &[
            "host",
            "upgrade",
            "--expect-binary",
            &binary(fixture),
            "--drain",
            "0",
        ],
        true,
        &[(ENABLED, "1")],
    )
}

fn commit_source(fixture: &Fixture, source: &mut Host) {
    let result = upgrade(fixture);
    assert!(!result.status.success(), "source commit cut did not fire");
    assert_eq!(receipts(fixture)[0]["phase"], "committed", "{result:?}");
    assert_eq!(
        runner::cut_exit(source.child.as_mut().unwrap()).code(),
        Some(86)
    );
    source.child = None;
}

fn crash_destination(fixture: &Fixture, cut: &str) {
    let (mut child, lines) = spawn_env(
        fixture,
        &[],
        &[
            (ENABLED, "1".into()),
            ("PLAK_TEST_STATE_KEY", DESTINATION.into()),
            ("PLAK_TEST_HOST_UPGRADE_CUT", cut.into()),
        ],
    );
    assert_eq!(
        runner::cut_exit(&mut child).code(),
        Some(86),
        "{cut}: {:?}",
        lines.try_iter().collect::<Vec<_>>()
    );
}

fn destination(fixture: &Fixture) -> Host {
    start_env(
        fixture,
        &[],
        &[
            (ENABLED, "1".into()),
            ("PLAK_TEST_STATE_KEY", DESTINATION.into()),
        ],
    )
}

fn native_count(fixture: &Fixture, query: &str) -> i64 {
    rusqlite::Connection::open_with_flags(
        fixture
            .home()
            .join("state")
            .join(DESTINATION)
            .join("runtime.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap()
    .query_row(query, [], |row| row.get(0))
    .unwrap()
}

fn allocation(fixture: &Fixture) -> serde_json::Value {
    let rows = runner::local_daemon_lines(fixture);
    let jobs: Vec<_> = rows
        .iter()
        .flat_map(|line| line.jobs["jobs"].as_array().unwrap())
        .collect();
    assert_eq!(
        jobs.len(),
        1,
        "retained inventory duplicated or lost an allocation"
    );
    let job = jobs[0];
    json!({"session_id":job["session_id"],"number":job["number"],"owner_generation":job["owner_generation"],"started_at":job["started_at"]})
}

fn stopped(pid: i32) {
    let deadline = Instant::now() + WAIT;
    while runner::alive(pid) {
        assert!(Instant::now() < deadline, "job did not stop");
        std::thread::sleep(Duration::from_millis(10));
    }
}
