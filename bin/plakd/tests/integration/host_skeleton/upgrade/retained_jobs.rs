//! Daemon-owned jobs survive host release and remain explicitly stoppable.

use super::*;

#[test]
fn paired_quiescent_job_survives_sigterm_and_restart() {
    paired_handoff(Handoff::Restart);
}

#[test]
fn paired_quiescent_job_survives_unchanged_triple_upgrade() {
    paired_handoff(Handoff::Upgrade);
}

#[test]
fn paired_job_survives_sigkill_and_readoption() {
    paired_handoff(Handoff::Crash);
}

#[derive(Clone, Copy)]
enum Handoff {
    Restart,
    Upgrade,
    Crash,
}

fn paired_handoff(handoff: Handoff) {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let (_daemon, path, id) = runner::remote_fixture(&fixture);
    let (mut host, _client, session, node) = reopen::placed_job(&fixture, &path, &id, false, false);
    let pid = runner::job(&fixture).0;
    let before = dormant::identity(&fixture, &plakd::home::state::state_key(), &session);
    settled_script(&fixture);
    match handoff {
        Handoff::Restart => {
            stop(&mut host);
            assert!(runner::alive(pid), "SIGTERM killed the daemon-owned job");
            host = start_env(&fixture, &[], &[("PATH", path.clone())]);
        }
        Handoff::Crash => {
            kill(&mut host);
            assert!(runner::alive(pid), "host loss killed the daemon-owned job");
            host = start_env(&fixture, &[], &[("PATH", path.clone())]);
        }
        Handoff::Upgrade => {
            let result = reopen::same_key_upgrade(&fixture, &path);
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
    }
    let mut client = Client::connect(&host);
    runner::remote_ready(&mut client, &id);
    client.attach(20, &node);
    assert!(runner::alive(pid), "handoff lost the running job");
    assert_eq!(runner::job(&fixture).0, pid);
    assert_eq!(
        drain::dispatches(&fixture),
        2,
        "handoff replayed a process call"
    );
    let after = dormant::identity(&fixture, &plakd::home::state::state_key(), &session);
    assert_eq!(after.0.session_id(), before.0.session_id());
    assert_eq!(after.0.branch_id(), before.0.branch_id());
    assert_eq!(after.1, before.1);
    stop_job(&mut client, session, pid);
    stop(&mut host);
}

#[test]
fn home_local_quiescent_job_survives_sigterm_and_restart() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let (mut host, session, node) = runner::start_job(&fixture, false);
    let pid = runner::job(&fixture).0;
    settled_script(&fixture);
    stop(&mut host);
    assert!(runner::alive(pid));
    let mut host = start(&fixture);
    runner::wait_local_runner_ready(&host);
    let mut client = Client::connect(&host);
    client.attach(20, &node);
    assert!(runner::alive(pid));
    assert_eq!(runner::job(&fixture).0, pid);
    assert_eq!(drain::dispatches(&fixture), 2);
    stop_job(&mut client, session, pid);
    stop(&mut host);
}

fn settled_script(fixture: &Fixture) {
    fixture.write_script(
        &json!({"version":1,"process_tools":true,"steps":[{"output":{"text":ANSWER}}]}),
    );
}

fn stop_job(client: &mut Client, session: String, pid: i32) {
    assert_eq!(
        client.request(21, Op::StopSession { session }),
        Outcome::Ok(Answer::Stopped {
            runner_pending: false
        })
    );
    let deadline = Instant::now() + WAIT;
    while runner::alive(pid) {
        assert!(
            Instant::now() < deadline,
            "explicit stop left the job running"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
