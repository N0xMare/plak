//! Startup policy and link-loss recovery at the product boundary.

use super::*;

mod journal;
mod restored;

#[test]
fn a_host_exits_nonzero_when_its_lost_daemon_cannot_restart() {
    let Some(fixture) = fixture() else { return };
    let mut host = start(&fixture);
    let version = plakous::runner::protocol::PROTOCOL_VERSION;
    std::fs::write(
        fixture
            .home()
            .join(format!("host/runner/v{version}/owners.json")),
        "not owner floors",
    )
    .unwrap();
    kill_local_daemon_only(&fixture.home());
    // Keep failed-redial teardown from racing another detached child start.
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .open(
            fixture
                .home()
                .join(format!("host/runner/v{version}/run/runner.lock")),
        )
        .unwrap();
    let lock_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {
                assert!(std::time::Instant::now() < lock_deadline);
                std::thread::yield_now();
            }
            Err(error) => panic!("lock probe: {error}"),
        }
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Some(status) = host.child.as_mut().unwrap().try_wait().unwrap() {
            assert!(!status.success());
            host.child.take();
            break;
        }
        if std::time::Instant::now() >= deadline {
            kill(&mut host);
            panic!("the host did not fail after bounded redial");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn a_refusing_obsolete_daemon_does_not_block_host_startup() {
    use plakous::runner::protocol::{Refusal, RunnerFrame, frame};
    let Some(fixture) = fixture() else { return };
    let run = fixture.home().join("host/runner/v1/run");
    std::fs::create_dir_all(&run).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(run.join("runner.sock")).unwrap();
    let refusal = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .write_all(
                &frame::encode(&RunnerFrame::Refused {
                    request_id: None,
                    refusal: Refusal::Malformed {
                        detail: "obsolete probe".into(),
                    },
                })
                .unwrap(),
            )
            .unwrap();
    });
    let mut host = start_env(&fixture, &[], &[("PLAK_LOG", "info".into())]);
    refusal.join().unwrap();
    Client::connect(&host).rows(1);
    stop(&mut host);
    let log = std::fs::read_to_string(fixture.home().join("logs/plak.log")).unwrap();
    assert!(
        log.contains("runner_recovery_failed") && log.contains("inspection"),
        "{log}"
    );
}

#[test]
fn a_startup_stop_never_takes_a_line_the_sweep_did_not_readopt() {
    let Some(fixture) = fixture() else { return };
    let (mut host, session, _) = start_job(&fixture, false);
    let (pid, _) = job(&fixture);
    kill(&mut host);
    sweep::record_stop(&fixture, &session);
    let mut host = start_env(
        &fixture,
        &[],
        &[("PLAK_TEST_HOST_READOPT_CUT", "skip-readopt".into())],
    );
    assert!(
        alive(pid),
        "a skipped line was stopped through a fresh lease"
    );
    kill(&mut host);
    let mut host = start(&fixture);
    stopped(pid);
    stop(&mut host);
}

#[test]
fn an_embedded_open_refuses_a_daemon_held_line_without_recording_a_terminal() {
    let Some(fixture) = fixture() else { return };
    let (mut host, session, node) = start_job(&fixture, false);
    let (pid, _) = job(&fixture);
    kill(&mut host);
    let before = unknown_effects(&fixture, &session);
    let outcome = fixture
        .command()
        .args(["agent", "--session", &session])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&outcome.stdout),
        String::from_utf8_lossy(&outcome.stderr)
    );
    assert_eq!(outcome.status.code(), Some(7), "{text}");
    assert!(
        text.contains("this session's processes run in the local runner"),
        "{text}"
    );
    assert!(alive(pid));
    assert_eq!(unknown_effects(&fixture, &session), before);
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let connection = rusqlite::Connection::open(paths.runtime_database()).unwrap();
    let terminals: u32 = connection
        .query_row("SELECT COUNT(*) FROM line_job_terminals", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(terminals, 0);
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    client.attach(1, &node);
    assert!(alive(pid));
    assert_eq!(
        client.request(
            2,
            Op::StopSession {
                session: session.clone()
            }
        ),
        Outcome::Ok(Answer::Stopped)
    );
    stopped(pid);
    stop(&mut host);
    kill_home_local_runner(&fixture.home());
    let mut embedded = AgentProcess::start(&fixture, &["--session", &session]);
    embedded.wait_for("embedded ready without daemon", |row| {
        row["kind"] == "ready"
    });
    embedded.send(&quit_frame());
    assert!(embedded.finish().status.success());
}

#[test]
fn a_slow_user_manager_is_started_once_without_a_detached_fallback() {
    for outcome in ["exit 0", "exit 1", "exec sleep 2"] {
        manager_start_case(outcome);
    }
}

fn manager_start_case(outcome: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    let Some(fixture) = fixture() else { return };
    let bin = fixture.root.join("manager-bin");
    std::fs::create_dir(&bin).unwrap();
    let counter = fixture.root.join("manager-calls");
    for (name, script) in [
        ("systemctl", "#!/bin/sh\nprintf 'running\\n'\n".to_owned()),
        (
            "systemd-run",
            format!(
                "#!/bin/sh\nn=$(cat '{c}' 2>/dev/null || echo 0)\necho $((n + 1)) > '{c}'\n[ \"$n\" = 0 ] || exit 1\nwhile [ \"$#\" -gt 0 ] && [ \"$1\" != \"--\" ]; do [ \"$n\" -lt 100 ] || exit 1; n=$((n + 1)); case \"$1\" in --setenv=*) export \"${{1#--setenv=}}\";; esac; shift; done\nshift\npython3 -c 'import os,sys,time; os.setsid(); time.sleep(0.3); os.execv(sys.argv[2], [sys.argv[1], *sys.argv[3:]])' '{m}' \"$@\" </dev/null >/dev/null 2>&1 &\necho $! > '{m}'\n{outcome}\n",
                c = counter.display(),
                m = fixture.root.join("manager-child.marker").display()
            ),
        ),
    ] {
        let path = bin.join(name);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut host = start_env(
        &fixture,
        &[],
        &[
            ("INVOCATION_ID", "slow-manager".into()),
            ("PLAK_TEST_RUNNER_DETACHED", "0".into()),
            ("PATH", format!("{}:/usr/bin:/bin", bin.display())),
        ],
    );
    assert_eq!(std::fs::read_to_string(counter).unwrap().trim(), "1");
    let lock = fixture.home().join(format!(
        "host/runner/v{}/run/runner.lock",
        plakous::runner::protocol::PROTOCOL_VERSION
    ));
    let pid: i32 = std::fs::read_to_string(lock).unwrap().parse().unwrap();
    let output = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let parent: u32 = String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_ne!(parent, host.child.as_ref().unwrap().id());
    kill(&mut host);
}

#[test]
fn a_lost_daemon_is_redialed_while_the_host_serves() {
    let Some(fixture) = fixture() else { return };
    let first = fixture.root.join("first-process.marker");
    let second = fixture.root.join("second-process.marker");
    let step = |marker: &std::path::Path| {
        json!({"output": {"tool": {
        "name": "process_session_start", "arguments": {
            "command": format!("printf started > '{}'; exit 0", marker.display()),
            "workdir": ".", "login": false, "tty": false, "keep_stdin_open": false,
            "yield_time_ms": 1000, "max_output_bytes": 8192
        }}}, "finish": "tool_calls"})
    };
    fixture.write_script(&json!({"version": 1, "process_tools": true, "steps": [
        step(&first), {"output": {"text": ANSWER}, "finish": "stop"},
        step(&second), {"output": {"text": CONTINUED}, "finish": "stop"}
    ]}));
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created")
    };
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, "first process", ANSWER);
    assert!(first.exists());
    kill_local_daemon_only(&fixture.home());
    client.prompt(4, attachment, "second process", CONTINUED);
    assert!(second.exists());
    stop(&mut host);
}

#[test]
fn a_busy_roots_child_job_survives_attach_after_host_loss() {
    for policy in ["continue", "hold"] {
        let Some(fixture) = fixture() else { return };
        write_resume_policy(&fixture, policy);
        let (mut host, session, node) = start_child_job_variant(&fixture, true);
        let (pid, _) = job(&fixture);
        hold_child_script(&fixture);
        kill(&mut host);
        let mut host = start(&fixture);
        let mut client = Client::connect(&host);
        client.attach(1, &node);
        std::thread::sleep(std::time::Duration::from_secs(5));
        assert!(
            alive(pid),
            "the held child lost its promoted job after attach"
        );
        assert_eq!(
            client.request(2, Op::StopSession { session }),
            Outcome::Ok(Answer::Stopped)
        );
        stopped(pid);
        stop(&mut host);
    }
}
