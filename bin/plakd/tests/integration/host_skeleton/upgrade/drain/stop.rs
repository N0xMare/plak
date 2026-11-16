//! An operator stop defeats reversible upgrade preparation, never becomes a handoff.

use super::*;

#[test]
fn host_step_drain_operator_stop_aborts_preparation_without_resuming_stopped_work() {
    operator_stop("1m");
}

#[test]
fn host_step_drain_idle_operator_stop_aborts_preparation_without_continuation() {
    operator_stop("idle");
}

fn operator_stop(mode: &str) {
    let Some(fixture) = fixture() else { return };
    let (mut host, mut client, session, _node, _release) = busy(&fixture);
    let _cleanup = HomeHost(fixture.home());
    let id = binary(&fixture);
    let mut command = fixture.command();
    command
        .args(["host", "upgrade", "--expect-binary", &id, "--drain", mode])
        .env("PLAK_TEST_FORCE_HOST_UPGRADE", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut requester = Requester(Some(command.spawn().unwrap()));
    let deadline = Instant::now() + WAIT;
    loop {
        let mut probe = new_session(&fixture.root.join("absent-workspace"));
        if let Op::NewSession { request_id, .. } = &mut probe {
            *request_id = uuid::Uuid::new_v4().to_string();
        }
        if let Outcome::Refused(reason) = client.request(10, probe)
            && format!("{reason:?}").contains("upgrad")
        {
            break;
        }
        assert!(Instant::now() < deadline, "upgrade never paused admissions");
        assert!(requester.0.as_mut().unwrap().try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(20));
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
    while requester.0.as_mut().unwrap().try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "stop did not abort the drain");
        std::thread::sleep(Duration::from_millis(10));
    }
    let result = requester.0.take().unwrap().wait_with_output().unwrap();
    assert!(!result.status.success());
    assert!(!fixture.home().join("host/upgrades").exists());
    assert_eq!(root_record(&fixture, &session).1, 0);
    assert!(matches!(
        plakous_hosted::runtime::root_standing(&store(&fixture), &session).unwrap(),
        plakous_hosted::runtime::RootStanding::Stopped
    ));
    assert_eq!(client.request(12, Op::Ping), Outcome::Ok(Answer::Pong));
    let mut create = new_session(&fixture.workspace());
    if let Op::NewSession { request_id, .. } = &mut create {
        *request_id = uuid::Uuid::new_v4().to_string();
    }
    assert!(
        matches!(
            client.request(13, create),
            Outcome::Ok(Answer::Created { .. })
        ),
        "abort did not reopen admissions"
    );
    assert_eq!(dispatches(&fixture), 1);
    stop(&mut host);
}
