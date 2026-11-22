//! Crash cuts straddle both the host's release and the receiver's owner CAS.

use super::*;
use plakous_hosted::test_support_runner::HostCut;

#[test]
fn resume_after_each_handoff_boundary_preserves_original_jobs() {
    for (flag, cut) in [
        ("PLAK_TEST_HOST_UPGRADE_CUT", "before-answer"),
        ("PLAK_TEST_HOST_UPGRADE_CUT", "after-source-release"),
        ("PLAK_TEST_HOST_READOPT_CUT", HostCut::Lease.name()),
        ("PLAK_TEST_HOST_READOPT_CUT", HostCut::Transfer.name()),
        ("PLAK_TEST_HOST_READOPT_CUT", HostCut::Terminals.name()),
    ] {
        crash(flag, cut, false);
    }
}

#[test]
fn original_daemon_loss_after_transfer_is_unknown_and_never_replays() {
    crash("PLAK_TEST_HOST_READOPT_CUT", HostCut::Transfer.name(), true);
}

fn crash(flag: &str, cut: &str, lose_original: bool) {
    let Some(fixture) = fixture() else { return };
    let fixture = fixture.with_session_lease_seconds(2);
    let _cleanup = HomeHost(fixture.home());
    let image = replacement_image(&fixture).canonicalize().unwrap();
    let environment = candidate_environment(&image);
    let mut source_environment = environment.to_vec();
    source_environment.push((flag, cut.into()));
    source_script(&fixture);
    let mut host = start_env(&fixture, &[], &source_environment);
    runner::wait_local_runner_ready(&host);
    let mut client = Client::connect(&host);
    let root = start_root(&fixture, &mut client, "local", None, 1);
    drop(client);
    destination_script(&fixture);
    let mut requester = TestChild::spawn(
        candidate_upgrade(&fixture, &image, &environment)
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );
    let status = runner::cut_exit(host.child.as_mut().unwrap());
    assert!(!status.success(), "cut {cut} exited normally");
    let _ = requester.kill();
    let _ = requester.wait();
    assert_eq!(marked(&fixture, &root.name, "bg"), root.background, "{cut}");
    assert_eq!(marked(&fixture, &root.name, "fg"), root.foreground, "{cut}");
    assert!(
        runner::alive(root.background.0) && runner::alive(root.foreground.0),
        "{cut}"
    );
    if lose_original {
        // Leave the original's stale socket in place, as SIGKILL would.
        let daemon = rustix::process::Pid::from_raw(root.foreground.1).unwrap();
        rustix::process::kill_process(daemon, rustix::process::Signal::KILL).unwrap();
        let lock = local_runner_root(&fixture.home()).join("run/r.lock");
        let lock = std::fs::File::options().write(true).open(lock).unwrap();
        let deadline = Instant::now() + WAIT;
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    assert!(Instant::now() < deadline, "old daemon still holds its lock");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => panic!("cannot inspect the old daemon lock: {err}"),
            }
        }
    }
    let mut replacement = await_serving_host(spawn_host(
        image_command(&fixture, &image)
            .envs(environment)
            .arg("host"),
    ))
    .unwrap();
    runner::wait_local_runner_ready(&replacement);
    if !lose_original {
        for job in ["bg", "fg"] {
            std::fs::write(fixture.root.join(format!("local-{job}-finish")), "").unwrap();
        }
        wait_exit(root.background.0);
        wait_exit(root.foreground.0);
    }
    release_continuation(&fixture);
    notices(&fixture, &root.session, !lose_original);
    assert_eq!(marked(&fixture, &root.name, "bg"), root.background);
    assert_eq!(marked(&fixture, &root.name, "fg"), root.foreground);
    stop(&mut replacement);
}
