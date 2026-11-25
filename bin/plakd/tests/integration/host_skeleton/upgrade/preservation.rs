use super::*;
use plakous_host_protocol::wire::RequestRefusal;

#[test]
fn a_held_capture_failure_withdraws_and_concurrent_requester_is_busy() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let release = fixture.root.join("release-drain");
    let mut host = start_env(
        &fixture,
        &[],
        &[(
            "PLAK_TEST_HOST_UPGRADE_DRAIN_GATE",
            release.display().to_string(),
        )],
    );
    runner::wait_local_runner_ready(&host);
    let mut client = Client::connect(&host);
    let id = binary(&fixture);
    let pid = host.child.as_ref().unwrap().id();
    let mut command = fixture.command();
    command
        .args(["host", "upgrade", "--expect-binary", &id, "--drain", "20"])
        .env("PLAK_TEST_FORCE_HOST_UPGRADE", "1");
    let (send, outcome) = mpsc::channel();
    std::thread::spawn(move || send.send(command.output()).unwrap());
    let deadline = Instant::now() + WAIT;
    loop {
        if matches!(
            client.request(10, new_session(&fixture.root.join("no-workspace"))),
            Outcome::Refused(RequestRefusal::Backpressure)
        ) {
            break;
        }
        assert!(Instant::now() < deadline, "upgrade never paused admissions");
        std::thread::sleep(Duration::from_millis(10));
    }
    let busy = output_env(
        &fixture,
        &["host", "upgrade", "--expect-binary", &id],
        false,
        &[("PLAK_TEST_FORCE_HOST_UPGRADE", "1")],
    );
    assert!(!busy.status.success());
    assert!(String::from_utf8_lossy(&busy.stderr).contains("host_upgrade_checkpoint_busy"));
    let config = fixture.home().join("config.toml");
    let saved = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        format!("{saved}\n[runtime_store]\nretire_finished_children = true\n"),
    )
    .unwrap();
    std::fs::write(release, []).unwrap();
    let result = outcome.recv_timeout(WAIT).unwrap().unwrap();
    let detail = String::from_utf8_lossy(&result.stderr);
    assert!(!result.status.success(), "{detail}");
    assert!(
        detail.contains("host_upgrade_preservation_refused: cannot verify intentional retirement"),
        "{detail}"
    );
    std::fs::write(config, saved).unwrap();
    assert_eq!(host.child.as_ref().unwrap().id(), pid);
    assert_eq!(client.request(11, Op::Ping), Outcome::Ok(Answer::Pong));
    let request_id = uuid::Uuid::new_v4().to_string();
    let created = client.request(
        12,
        new_session_request(&fixture.workspace(), None, &request_id),
    );
    assert!(
        matches!(created, Outcome::Ok(Answer::Created { .. })),
        "{created:?}"
    );
    assert!(!fixture.home().join("host/upgrade-checkpoint").exists());
    stop(&mut host);
}

pub(in super::super) fn replace_store_identity(fixture: &Fixture) {
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let connection = rusqlite::Connection::open(paths.product_database()).unwrap();
    connection.busy_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        connection
            .execute(
                "UPDATE product_schema SET instance_id=?1",
                [uuid::Uuid::new_v4().to_string()],
            )
            .unwrap(),
        1
    );
}
