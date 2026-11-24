//! Startup policy and link-loss recovery at the product boundary.

use super::*;

mod journal;
mod restored;

#[test]
fn a_broken_daemon_blocks_local_placement_without_terminating_the_host() {
    let Some(fixture) = fixture() else { return };
    let mut host = start(&fixture);
    wait_local_runner_ready(&host);
    let floors = local_runner_root(&fixture.home()).join("owners.json");
    // An empty daemon has no owner floors until its first transfer. Corrupt
    // the recorded location deliberately rather than waiting for a transfer.
    std::fs::write(floors, "not owner floors").unwrap();
    kill_local_daemon_only(&fixture.home());
    // Keep failed-redial teardown from racing another detached child start.
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .open(local_runner_root(&fixture.home()).join("run/r.lock"))
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
    wait_for_recovery_failure(&fixture, &mut host, "owners.json");
    assert_eq!(Client::connect(&host).rows(1), []);
    stop(&mut host);
    for detached in ["0", "1"] {
        let mut host = start_env(
            &fixture,
            &[],
            &[("PLAK_TEST_RUNNER_DETACHED", detached.into())],
        );
        wait_for_recovery_failure(&fixture, &mut host, "owners.json");
        assert_eq!(Client::connect(&host).rows(1), []);
        stop(&mut host);
    }
}

#[test]
fn a_transient_daemon_failure_keeps_the_redial_bound() {
    for detached in ["0", "1"] {
        let Some(fixture) = fixture() else { return };
        let mut host = start_env(
            &fixture,
            &[],
            &[
                ("PLAK_TEST_RUNNER_DETACHED", detached.into()),
                ("PLAK_TEST_RUNNER_RECONNECT_SECONDS", "2".into()),
            ],
        );
        wait_local_runner_ready(&host);
        let socket = local_runner_root(&fixture.home()).join("run/r.sock");
        std::fs::remove_file(&socket).unwrap();
        // A listener that never sends Hello is transient, not a start refusal.
        let _listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
        kill_local_daemon_only(&fixture.home());
        wait_for_recovery_failure(&fixture, &mut host, "did not reconnect within 2 s");
        assert_eq!(Client::connect(&host).rows(1), []);
        stop(&mut host);
    }
}

fn wait_for_recovery_failure(fixture: &Fixture, host: &mut Host, detail: &str) {
    use plakous_host_protocol::wire::{PlacementReason, RunnerPhase, RunnerStatus};
    let mut client = Client::connect(host);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
    let mut diagnostics = String::new();
    loop {
        assert!(
            host.child.as_mut().unwrap().try_wait().unwrap().is_none(),
            "{diagnostics}"
        );
        for line in host.lines.try_iter() {
            diagnostics.push_str(&line);
        }
        let log = std::fs::read_to_string(fixture.home().join("logs/plak.log")).unwrap_or_default();
        if (diagnostics.contains("runner_recovery_failed") && diagnostics.contains(detail))
            || (log.contains("runner_recovery_failed") && log.contains(detail))
        {
            // Logs persist across restarts; require this host's live verdict,
            // rather than accepting only a predecessor's matching diagnostic.
            if matches!(
                local_runner_status(&mut client),
                RunnerStatus::Blocked {
                    reason: PlacementReason::RunnerUnreachable {
                        phase: RunnerPhase::Daemon,
                        ..
                    }
                }
            ) {
                return;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "missing bounded runner failure: {diagnostics}\n{log}"
        );
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
        &[(
            "PLAK_TEST_HOST_READOPT_CUT",
            HostCut::SkipReadopt.name().into(),
        )],
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

/// A running daemon-owned process requires explicit ownership transfer.
/// Refusal must not fabricate a terminal or prevent the next host from adopting.
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
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
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
        Outcome::Ok(Answer::Stopped {
            runner_pending: false
        })
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
fn detached_daemon_and_piped_job_do_not_ignore_interrupt_or_quit() {
    let Some(fixture) = fixture() else { return };
    let (mut host, _, _) = start_job_env(
        &fixture,
        false,
        &[("PLAK_TEST_RUNNER_DETACHED", "1".into())],
    );
    let (job, daemon) = job(&fixture);
    let host_pid = host.child.as_ref().unwrap().id();
    let parent = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &daemon.to_string()])
        .output()
        .unwrap();
    assert!(parent.status.success());
    let parent: u32 = std::str::from_utf8(&parent.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    #[cfg(target_os = "linux")]
    let masks = [
        ("detached daemon", ignored_signals(daemon)),
        ("piped job", ignored_signals(job)),
    ];
    // macOS exposes no ignored-signal set. A piped job inherits any ignored
    // disposition from the daemon, so INT ending the job and QUIT ending the
    // daemon show neither process ignores either signal.
    #[cfg(target_os = "macos")]
    {
        terminates_on(job, nix::sys::signal::Signal::SIGINT, "piped job");
        terminates_on(daemon, nix::sys::signal::Signal::SIGQUIT, "detached daemon");
    }
    stop(&mut host);
    kill_home_local_runner(&fixture.home());

    assert_ne!(
        parent, host_pid,
        "the host still owns the daemon's wait status"
    );
    #[cfg(target_os = "linux")]
    for (process, mask) in masks {
        for signal in [rustix::process::Signal::INT, rustix::process::Signal::QUIT] {
            assert_eq!(
                mask & (1 << (signal.as_raw() - 1)),
                0,
                "{process} ignores {signal:?}"
            );
        }
    }
}

#[cfg(target_os = "linux")]
fn ignored_signals(pid: i32) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let ignored = status
        .lines()
        .find_map(|line| line.strip_prefix("SigIgn:"))
        .unwrap();
    u64::from_str_radix(ignored.trim(), 16).unwrap()
}

#[cfg(target_os = "macos")]
fn terminates_on(pid: i32, signal: nix::sys::signal::Signal, process: &str) {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), signal).unwrap();
    let deadline = std::time::Instant::now() + WAIT;
    while alive(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "{process} ignores {signal:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
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
    wait_local_runner_ready(&host);
    assert_eq!(std::fs::read_to_string(counter).unwrap().trim(), "1");
    let lock = local_runner_root(&fixture.home()).join("run/r.lock");
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
    use std::os::unix::process::ExitStatusExt as _;
    for (phase, policy) in [
        ("crash", "continue"),
        ("crash", "hold"),
        ("handoff", "hold"),
        ("before-root-handoff", "continue"),
        ("before-root-handoff", "hold"),
        ("after-root-handoff", "continue"),
    ] {
        let Some(fixture) = fixture() else { return };
        write_resume_policy(&fixture, policy);
        let env = if phase == "before-root-handoff" {
            vec![(
                "PLAK_TEST_HOST_READOPT_CUT",
                HostCut::BeforeRootHandoff.name().into(),
            )]
        } else {
            Vec::new()
        };
        let (mut host, session, node) = start_child_job_variant(&fixture, true, &env);
        let (pid, _) = job(&fixture);
        hold_child_script(&fixture);
        match phase {
            "crash" => kill(&mut host),
            "handoff" => stop(&mut host),
            _ => {
                nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(host.child.as_ref().unwrap().id().cast_signed()),
                    nix::sys::signal::Signal::SIGTERM,
                )
                .unwrap();
                if phase == "before-root-handoff" {
                    assert_eq!(cut_exit(host.child.as_mut().unwrap()).signal(), Some(9));
                    host.child = None;
                    assert!(
                        !root_record(&fixture, &session)
                            .0
                            .contains(&RootInterruptCause::Handoff)
                    );
                } else {
                    let deadline = std::time::Instant::now() + WAIT;
                    while !root_record(&fixture, &session)
                        .0
                        .contains(&RootInterruptCause::Handoff)
                    {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "the busy root never handed off"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    kill(&mut host);
                }
            }
        }
        assert!(alive(pid), "{phase}: the child process died during handoff");
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
            Outcome::Ok(Answer::Stopped {
                runner_pending: false
            })
        );
        stopped(pid);
        stop(&mut host);
    }
}
#[test]
fn a_local_daemon_does_not_recreate_a_deleted_home() {
    let Some(fixture) = fixture() else { return };
    let mut command = fixture.command();
    command
        .args(["runner", "serve", "--local"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    std::fs::remove_dir_all(fixture.home()).unwrap();
    let mut child = Child::spawn(&mut command);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let failed = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break !status.success();
        }
        if std::time::Instant::now() >= until {
            break false;
        }
        std::thread::yield_now();
    };
    drop(child);
    assert!(failed, "the daemon served after its home was removed");
    assert!(
        !fixture.home().exists(),
        "daemon startup resurrected its home"
    );
}
