//! A stop during a running hold settles in the source and does not cancel a paired upgrade.

use super::*;

#[test]
fn host_step_drain_operator_stop_settles_without_resuming_stopped_work() {
    operator_control("1m", false);
}

#[test]
fn host_step_drain_idle_operator_stop_reaches_the_boundary_without_continuation() {
    operator_control("idle", false);
}

#[test]
fn host_step_drain_idle_operator_interrupt_keeps_the_upgrade() {
    operator_control("idle", true);
}

fn operator_control(mode: &str, interrupt: bool) {
    let Some(fixture) = fixture() else { return };
    let (mut host, mut client, session, node, _release) = busy(&fixture);
    let _cleanup = HomeHost(fixture.home());
    let id = binary(&fixture);
    let progress = fixture.root.join("upgrade-progress.log");
    let mut command = fixture.command();
    command
        .args(["host", "upgrade", "--expect-binary", &id, "--drain", mode])
        .env("PLAK_TEST_FORCE_HOST_UPGRADE", "1")
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(&progress).unwrap());
    let mut requester = Requester(Some(command.spawn().unwrap()));
    let deadline = Instant::now() + WAIT;
    loop {
        let status = std::fs::read_to_string(&progress).unwrap();
        if status.contains("model=1") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "upgrade never held the running owner: {status}"
        );
        assert!(requester.0.as_mut().unwrap().try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(client.request(10, Op::Ping), Outcome::Ok(Answer::Pong));
    if interrupt {
        let attachment = client.attach(11, &node);
        assert_eq!(
            client.request(
                12,
                Op::Intent {
                    attachment,
                    intent: SessionIntent::Interrupt,
                    client_id: None,
                    blobs: Vec::new(),
                }
            ),
            Outcome::Ok(Answer::Accepted { duplicate: false })
        );
    } else {
        assert_eq!(
            client.request(
                11,
                Op::StopSession {
                    session: session.clone()
                }
            ),
            Outcome::Ok(Answer::Stopped {
                runner_pending: false
            }),
        );
    }
    while requester.0.as_mut().unwrap().try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "stop did not finish the drain");
        std::thread::sleep(Duration::from_millis(10));
    }
    let result = requester.0.take().unwrap().wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        std::fs::read_to_string(progress).unwrap()
    );
    assert!(!fixture.home().join("host/upgrades").exists());
    assert_eq!(root_record(&fixture, &session).1, 0);
    if !interrupt {
        assert!(matches!(
            plakous_hosted::runtime::root_standing(&store(&fixture), &session).unwrap(),
            plakous_hosted::runtime::RootStanding::Stopped
        ));
    }
    let mut successor = Client::connect(&host);
    assert_eq!(successor.request(12, Op::Ping), Outcome::Ok(Answer::Pong));
    assert_eq!(dispatches(&fixture), 1);
    stop(&mut host);
}
