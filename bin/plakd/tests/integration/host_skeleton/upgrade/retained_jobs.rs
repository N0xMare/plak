//! Daemon-owned jobs survive host release and remain explicitly stoppable.

use super::*;

pub(super) mod changed;

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
    let (mut host, _client, session, node) = reopen::placed_job(&fixture, &path, &id, false);
    let pid = runner::job(&fixture).0;
    let before = identity(&fixture, &plakd::home::state::state_key(), &session);
    let job = running_job(&fixture, &session);
    settled_script(&fixture);
    match handoff {
        Handoff::Restart => {
            stop(&mut host);
            terminal_rows(&fixture, 0);
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
    terminal_rows(&fixture, 0);
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
    let after = identity(&fixture, &plakd::home::state::state_key(), &session);
    assert_eq!(after.0.session_id(), before.0.session_id());
    assert_eq!(after.0.branch_id(), before.0.branch_id());
    assert_eq!(after.1, before.1);
    assert_eq!(running_job(&fixture, &session), job);
    stop_job(&mut client, session, pid);
    stop(&mut host);
    terminal_rows(&fixture, 1);
}

#[test]
fn home_local_quiescent_job_survives_sigterm_and_restart() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let (mut host, session, node) = runner::start_job(&fixture, false);
    let pid = runner::job(&fixture).0;
    let job = running_job(&fixture, &session);
    settled_script(&fixture);
    stop(&mut host);
    terminal_rows(&fixture, 0);
    assert!(runner::alive(pid));
    let mut host = start(&fixture);
    runner::wait_local_runner_ready(&host);
    let mut client = Client::connect(&host);
    client.attach(20, &node);
    assert!(runner::alive(pid));
    assert_eq!(runner::job(&fixture).0, pid);
    assert_eq!(drain::dispatches(&fixture), 2);
    assert_eq!(running_job(&fixture, &session), job);
    stop_job(&mut client, session, pid);
    stop(&mut host);
    terminal_rows(&fixture, 1);
}

fn running_job(fixture: &Fixture, session: &str) -> (u32, u64) {
    let (head, _) = identity(fixture, &plakd::home::state::state_key(), session);
    let jobs = plakous_offline_crash_harness::ledger_signature::inspect_jobs(
        &plakd::home::paths::PlakPaths::explicit(fixture.home()).runtime_database(),
        head,
    )
    .unwrap();
    assert_eq!(jobs.len(), 1, "handoff lost the durable job row");
    assert!(!jobs[0].terminal, "handoff recorded a terminal job");
    terminal_rows(fixture, 0);
    (jobs[0].number, jobs[0].owner_generation)
}

fn terminal_rows(fixture: &Fixture, expected: usize) {
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let db = rusqlite::Connection::open_with_flags(
        paths.runtime_database(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    if expected == 0 {
        let detached: i64 = db
            .query_row("SELECT count(*) FROM line_job_terminals", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(detached, 0, "handoff recorded a detached job terminal");
    }
    let head: String = db
        .query_row(
            "SELECT session_id || '/' || branch_id || '/' || checkpoint_id FROM branches",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let jobs = plakous_offline_crash_harness::ledger_signature::inspect_jobs(
        &paths.runtime_database(),
        head.parse().unwrap(),
    )
    .unwrap();
    assert_eq!(jobs.len(), 1, "stop lost its durable job row");
    let terminals = jobs.iter().filter(|job| job.terminal).collect::<Vec<_>>();
    assert_eq!(
        terminals.len(),
        expected,
        "unexpected durable job terminals"
    );
    for row in terminals {
        assert_eq!(row.number, 1);
        assert!(row.user_stop, "terminal must be attributed to the user");
    }
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
