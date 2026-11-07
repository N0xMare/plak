//! Daemon-held jobs survive the serving host and keep its start environment.

use super::*;

mod recovery;

#[test]
fn an_eof_before_the_local_runner_hello_is_retried() {
    for protocol in [1, plakous::runner::protocol::PROTOCOL_VERSION] {
        let Some(fixture) = fixture() else { return };
        let directory = fixture.home().join(format!("host/runner/v{protocol}/run"));
        std::fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("runner.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let dropped = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            drop(listener);
            std::fs::remove_file(socket).unwrap();
            drop(stream);
        });
        let mut host = start_env(&fixture, &[], &[("PLAK_LOG", "info".into())]);
        dropped.join().unwrap();
        stop(&mut host);
        let log = std::fs::read_to_string(fixture.home().join("logs/plak.log")).unwrap_or_default();
        assert!(
            !log.contains("runner_recovery_failed"),
            "a normal idle EOF is not a recovery failure: {log}"
        );
    }
}

#[test]
fn an_upgraded_host_inspects_the_old_protocol_and_starts_a_separate_daemon() {
    use plakous::runner::protocol::{
        HostFrame as RunnerHostFrame, RunnerFrame, VersionTriple, frame,
    };
    use plakous::tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId,
    };
    use std::io::Read as _;
    let Some(fixture) = fixture() else { return };
    let directory = fixture.home().join("host/runner/v1/run");
    std::fs::create_dir_all(&directory).unwrap();
    let socket = directory.join("runner.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000001".parse().unwrap()),
        fixture.workspace(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )
    .unwrap();
    let mut version = VersionTriple::current("old-revision", "old-flavor");
    version.protocol = 1;
    let old = plakous::runner::RunnerService::new(
        environment,
        plakous::runner::RunnerIdentity {
            runner_id: RunnerId::new("old-home").unwrap(),
            version,
            boot_id: "old-boot".into(),
            binary_id: None,
        },
    )
    .unwrap();
    let hello = old.hello().clone();
    let inspect = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + WAIT;
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "no obsolete-protocol inspection"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("old socket: {error}"),
            }
        };
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.set_nonblocking(false).unwrap();
        stream
            .write_all(&frame::encode(&RunnerFrame::Hello(hello)).unwrap())
            .unwrap();
        let RunnerHostFrame::Bind(bind) = runner_frame(&mut stream) else {
            panic!("not bound")
        };
        assert!(bind.inspect);
        assert_eq!(bind.version.protocol, 1);
        let RunnerHostFrame::Lines { request_id } = runner_frame(&mut stream) else {
            panic!("not inspecting lines")
        };
        stream
            .write_all(
                &frame::encode(&RunnerFrame::Lines {
                    request_id,
                    lines: Vec::new(),
                })
                .unwrap(),
            )
            .unwrap();
        let mut byte = [0];
        assert_eq!(stream.read(&mut byte).unwrap(), 0, "an inspector sent work");
    });
    let mut host = start(&fixture);
    inspect.join().unwrap();
    assert!(socket.exists());
    assert!(
        fixture
            .home()
            .join(format!(
                "host/runner/v{}/run/runner.sock",
                plakous::runner::protocol::PROTOCOL_VERSION
            ))
            .exists()
    );
    stop(&mut host);
}

fn runner_frame(stream: &mut UnixStream) -> plakous::runner::protocol::HostFrame {
    use std::io::Read as _;
    let mut prefix = [0; 4];
    stream.read_exact(&mut prefix).unwrap();
    let mut bytes = vec![0; u32::from_be_bytes(prefix).try_into().unwrap()];
    stream.read_exact(&mut bytes).unwrap();
    plakous::runner::protocol::frame::decode(&bytes).unwrap()
}

#[test]
fn a_starter_losing_to_the_exiting_daemons_lock_is_retried() {
    let Some(fixture) = fixture() else { return };
    let home = fixture.home().join(format!(
        "host/runner/v{}",
        plakous::runner::protocol::PROTOCOL_VERSION
    ));
    std::fs::create_dir_all(home.join("run")).unwrap();
    let lock = std::fs::File::create(home.join("run/runner.lock")).unwrap();
    lock.try_lock().unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let spawned_while_locked = home.join("serve.log").exists();
        drop(lock);
        assert!(
            !spawned_while_locked,
            "a daemon was spawned while its lock was held"
        );
    });
    let mut host = start(&fixture);
    release.join().unwrap();
    stop(&mut host);
}

#[test]
fn an_unreachable_user_manager_falls_back_to_a_detached_runner() {
    use std::os::unix::fs::PermissionsExt as _;
    let Some(fixture) = fixture() else { return };
    let bin = fixture.root.join("manager-bin");
    std::fs::create_dir(&bin).unwrap();
    let marker = fixture.root.join("manager-called");
    let manager = bin.join("systemctl");
    std::fs::write(
        &manager,
        format!(
            "#!/bin/sh\nprintf called > '{}'\nprintf 'offline\\n'\nexit 1\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&manager, std::fs::Permissions::from_mode(0o755)).unwrap();
    let rejected = bin.join("systemd-run");
    std::fs::write(&rejected, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&rejected, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut host = start_env(
        &fixture,
        &[],
        &[
            ("INVOCATION_ID", "unreachable".into()),
            ("PLAK_TEST_RUNNER_DETACHED", "0".into()),
            ("PATH", format!("{}:/usr/bin:/bin", bin.display())),
        ],
    );
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "called");
    stop(&mut host);
}

fn job_script(fixture: &Fixture, busy: bool) -> serde_json::Value {
    let marker = fixture.root.join("local-job.marker");
    let quoted = marker.display().to_string().replace('\'', "'\\''");
    let last = if busy {
        json!({"stream": [{"text": PREFIX}, {"stall": 30000}]})
    } else {
        json!({"output": {"text": ANSWER}, "finish": "stop"})
    };
    json!({"version": 1, "process_tools": true, "steps": [
        {"output": {"tool": {"name": "process_session_start", "arguments": {
            "command": format!("printf '%s %s\\n' \"$$\" \"$PPID\" >> '{quoted}'; read line"),
            "workdir": ".", "login": false, "tty": false, "keep_stdin_open": true,
            "yield_time_ms": 0, "max_output_bytes": 8192
        }}}, "finish": "tool_calls"}, last
    ]})
}

fn job(fixture: &Fixture) -> (i32, i32) {
    let marker = std::fs::read_to_string(fixture.root.join("local-job.marker")).unwrap();
    assert_eq!(marker.lines().count(), 1, "the job ran twice");
    let (pid, parent) = marker.trim().split_once(' ').unwrap();
    (pid.parse().unwrap(), parent.parse().unwrap())
}

fn alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

fn stopped(pid: i32) {
    let deadline = std::time::Instant::now() + WAIT;
    while alive(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "a stopped job is still running"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn cut_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the host did not reach its crash boundary");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn start_job(fixture: &Fixture, busy: bool) -> (Host, String, NodeAddress) {
    start_job_env(fixture, busy, &[])
}

fn start_job_env(
    fixture: &Fixture,
    busy: bool,
    env: &[(&str, String)],
) -> (Host, String, NodeAddress) {
    fixture.write_script(&job_script(fixture, busy));
    let host = start_env(fixture, &[], env);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created")
    };
    let attachment = client.attach(2, &node);
    client.prompt(
        3,
        attachment,
        "start one job",
        if busy { PREFIX } else { ANSWER },
    );
    let (pid, daemon) = job(fixture);
    assert!(alive(pid));
    assert_ne!(
        daemon.cast_unsigned(),
        host.child.as_ref().unwrap().id(),
        "the host, not the daemon, started the job"
    );
    (host, session, node)
}

#[test]
fn runner_job_survives_host_loss_and_is_readopted_before_attach() {
    let Some(fixture) = fixture() else { return };
    let (mut host, session, node) = start_job(&fixture, false);
    let (pid, daemon) = job(&fixture);
    kill(&mut host);
    assert!(
        alive(pid) && alive(daemon),
        "host loss killed the daemon's job"
    );
    let refused = embedded(&fixture, &session).expect_err("an embedded face took a running job");
    assert_eq!(refused["code"], "local_runner_processes", "{refused}");
    let mut host = start(&fixture);
    assert!(alive(pid));
    let mut client = Client::connect(&host);
    let _ = client.attach(1, &node);
    assert!(alive(pid));
    job(&fixture);
    assert_eq!(
        client.request(2, Op::StopSession { session }),
        Outcome::Ok(Answer::Stopped)
    );
    stopped(pid);
    stop(&mut host);
}

#[test]
fn a_foreground_host_cut_stops_at_transfer_and_records_unknown_once() {
    use std::os::unix::process::ExitStatusExt as _;
    for policy in ["hold", "continue"] {
        let Some(fixture) = fixture() else { return };
        let config = fixture.home().join("config.toml");
        let text = std::fs::read_to_string(&config).unwrap();
        std::fs::write(
            config,
            format!("{text}\n[host]\nresume_after_loss = \"{policy}\"\n"),
        )
        .unwrap();
        let (mut host, session, node) = start_foreground(&fixture);
        let (pid, _) = job(&fixture);
        kill(&mut host);
        assert!(alive(pid));
        // Stop completed, but its transfer answer has not reached a journal.
        for _ in 0..2 {
            let (mut child, _) = spawn_env(
                &fixture,
                &[],
                &[("PLAK_TEST_HOST_READOPT_CUT", "transfer".into())],
            );
            assert_eq!(cut_exit(&mut child).signal(), Some(9));
            stopped(pid);
        }
        let mut host = start(&fixture);
        stopped(pid);
        let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
        let connection = rusqlite::Connection::open(paths.runtime_database()).unwrap();
        let count: i64 = connection
            .query_row("SELECT count(*) FROM line_job_terminals", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "an unpromoted process is not a durable job");
        let mut client = Client::connect(&host);
        client.attach(1, &node);
        assert_eq!(unknown_effects(&fixture, &session), 1);
        kill(&mut host);
        let mut host = start(&fixture);
        let mut client = Client::connect(&host);
        client.attach(1, &node);
        assert_eq!(
            unknown_effects(&fixture, &session),
            1,
            "a recut settled the call twice"
        );
        job(&fixture);
        stop(&mut host);
    }
}

fn start_foreground(fixture: &Fixture) -> (Host, String, NodeAddress) {
    let mut script = job_script(fixture, false);
    script["steps"][0]["output"]["tool"]["arguments"]["yield_time_ms"] = json!(30000);
    let command = script["steps"][0]["output"]["tool"]["arguments"]["command"]
        .as_str()
        .unwrap()
        .replace(
            "read line",
            "n=0; while :; do [ \"$n\" -lt 300 ] || exit 1; n=$((n + 1)); sleep 1; done",
        );
    script["steps"][0]["output"]["tool"]["arguments"]["command"] = json!(command);
    fixture.write_script(&script);
    let host = start(fixture);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created")
    };
    let attachment = client.attach(2, &node);
    assert_eq!(
        client.request(
            3,
            Op::Intent {
                attachment,
                intent: SessionIntent::Prompt(Draft::from("start foreground")),
                blobs: Vec::new()
            }
        ),
        Outcome::Ok(Answer::Accepted)
    );
    let deadline = std::time::Instant::now() + WAIT;
    while !fixture.root.join("local-job.marker").exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    (host, session, node)
}

#[test]
fn a_runner_restart_leaves_an_unpromoted_call_unknown_without_restarting_it() {
    let Some(fixture) = fixture() else { return };
    let (mut host, session, node) = start_foreground(&fixture);
    let (pid, _) = job(&fixture);
    kill(&mut host);
    kill_local_daemon_only(&fixture.home());
    assert!(
        alive(pid),
        "this fixture must cover a runner's surviving orphan"
    );
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    client.attach(1, &node);
    assert_eq!(unknown_effects(&fixture, &session), 1);
    job(&fixture);
    kill(&mut host);
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    client.attach(1, &node);
    assert_eq!(unknown_effects(&fixture, &session), 1);
    stop(&mut host);
    assert!(alive(pid), "the new runner must not act on an old PID");
    // A dead runner has no safe identifier authority over its old child.
    // Fixture-owned marker identity, never session state, reaps it instead.
    drop(fixture);
    stopped(pid);
}

fn unknown_effects(fixture: &Fixture, session: &str) -> usize {
    use plak::store::session_records::SessionRecordStore as _;
    use plakous::sessions::feed::{
        FeedCursor, FeedFact, FeedOperationKind, FeedOperationState, FeedPage, FeedPageRequest,
        FeedSeq,
    };
    use plakous::sessions::{AgentSessions, SessionBackend};
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let product = plak::store::session_records::SqliteSessionStore::new(paths.clone());
    let locator = product.load(session).unwrap().plakous_locator.unwrap();
    let sessions =
        AgentSessions::new(SessionBackend::sqlite(paths.runtime_database()).unwrap()).unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut cursor = FeedSeq::new(0);
            let mut count = 0;
            loop {
                let FeedPage::Records { records, .. } = sessions
                    .feed_page(FeedPageRequest::after(FeedCursor::new(
                        locator.session_id(),
                        cursor,
                    )))
                    .await
                    .unwrap()
                else {
                    panic!("unexpected trimmed feed")
                };
                if records.is_empty() {
                    return count;
                }
                for record in records {
                    count += record
                        .facts()
                        .iter()
                        .filter(|fact| {
                            matches!(
                                fact,
                                FeedFact::OperationAdvanced {
                                    kind: FeedOperationKind::Effect { .. },
                                    state: FeedOperationState::OutcomeUnknown,
                                    ..
                                }
                            )
                        })
                        .count();
                    cursor = record.header().seq;
                }
            }
        })
}

#[test]
fn host_handoff_keeps_jobs_and_continues_the_busy_turn_once() {
    for policy in ["hold", "continue"] {
        handoff_keeps_jobs(policy);
    }
}

#[test]
fn an_idle_handoff_does_not_resurrect_a_previously_cancelled_root() {
    use std::os::unix::process::ExitStatusExt as _;
    for policy in ["hold", "continue"] {
        let Some(fixture) = fixture() else { return };
        let config = fixture.home().join("config.toml");
        let text = std::fs::read_to_string(&config).unwrap();
        std::fs::write(
            config,
            format!("{text}\n[host]\nresume_after_loss = \"{policy}\"\n"),
        )
        .unwrap();
        let (mut host, session, node) = start_job_env(
            &fixture,
            true,
            &[("PLAK_TEST_HOST_READOPT_CUT", "before-root-handoff".into())],
        );
        let (pid, _) = job(&fixture);
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(host.child.as_ref().unwrap().id().cast_signed()),
            nix::sys::signal::Signal::SIGTERM,
        )
        .unwrap();
        assert_eq!(cut_exit(host.child.as_mut().unwrap()).signal(), Some(9));
        host.child = None;
        let mut host = start(&fixture);
        let mut client = Client::connect(&host);
        client.attach(1, &node);
        stop(&mut host);
        assert!(alive(pid));
        assert!(
            !root_record(&fixture, &session)
                .0
                .contains(&RootInterruptCause::Handoff),
            "idle release claimed an earlier host's cancelled turn"
        );
        let mut host = start(&fixture);
        let mut client = Client::connect(&host);
        client.attach(1, &node);
        assert_eq!(
            client.request(2, Op::StopSession { session }),
            Outcome::Ok(Answer::Stopped)
        );
        stopped(pid);
        stop(&mut host);
    }
}

fn handoff_keeps_jobs(policy: &str) {
    let Some(fixture) = fixture() else { return };
    let config = fixture.home().join("config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        config,
        format!("{text}\n[host]\nresume_after_loss = \"{policy}\"\n"),
    )
    .unwrap();
    let (mut host, session, node) = start_job(&fixture, true);
    let (pid, daemon) = job(&fixture);
    stop(&mut host);
    assert!(
        alive(pid) && alive(daemon),
        "handoff stopped a daemon-held job"
    );
    assert_eq!(
        root_record(&fixture, &session),
        (vec![RootInterruptCause::Handoff], 0)
    );
    fixture.write_script(&json!({"version": 1, "process_tools": true,
        "steps": [{"output": {"text": CONTINUED}, "finish": "stop"}]}));
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    client.attach(1, &node);
    let deadline = std::time::Instant::now() + WAIT;
    while root_record(&fixture, &session).1 == 0 {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(alive(pid), "the continuation lost the running job");
    job(&fixture);
    stop(&mut host);
    assert!(alive(pid));
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    client.rows(1);
    client.attach(3, &node);
    assert_eq!(root_record(&fixture, &session).1, 1);
    assert_eq!(
        client.request(2, Op::StopSession { session }),
        Outcome::Ok(Answer::Stopped)
    );
    stopped(pid);
    stop(&mut host);
}

#[test]
fn a_childs_daemon_job_survives_host_loss_and_handoff() {
    let Some(probe) = fixture() else { return };
    drop(probe);
    for (handoff, policy) in [
        (false, "continue"),
        (false, "hold"),
        (true, "continue"),
        (true, "hold"),
    ] {
        let fixture = fixture().unwrap();
        write_resume_policy(&fixture, policy);
        let (mut host, session, node) = start_child_job(&fixture);
        let (pid, daemon) = job(&fixture);
        hold_child_script(&fixture);
        assert!(alive(pid));
        assert_ne!(daemon.cast_unsigned(), host.child.as_ref().unwrap().id());
        if handoff {
            stop(&mut host);
        } else {
            kill(&mut host);
        }
        assert!(alive(pid), "the child's job died with its host");
        let mut host = start(&fixture);
        assert!(alive(pid), "the sweep stopped the child's job");
        let mut client = Client::connect(&host);
        client.attach(1, &node);
        std::thread::sleep(std::time::Duration::from_secs(5));
        assert!(alive(pid));
        job(&fixture);
        assert_eq!(
            client.request(2, Op::StopSession { session }),
            Outcome::Ok(Answer::Stopped)
        );
        stopped(pid);
        stop(&mut host);
    }
}

fn start_child_job(fixture: &Fixture) -> (Host, String, NodeAddress) {
    start_child_job_variant(fixture, false)
}

fn write_resume_policy(fixture: &Fixture, policy: &str) {
    let config = fixture.home().join("config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        config,
        format!("{text}\n[host]\nresume_after_loss = \"{policy}\"\n"),
    )
    .unwrap();
}

fn hold_child_script(fixture: &Fixture) {
    // A fresh offline binding answers the continuation, not a second start.
    std::fs::write(fixture.root.join("child-script.json"), serde_json::to_vec(&json!({
        "version": 1, "steps": [{"stream": [{"gate": {"file": fixture.root.join("never")}}], "output": {"text": "child held"}}]
    })).unwrap()).unwrap();
}

fn start_child_job_variant(fixture: &Fixture, busy: bool) -> (Host, String, NodeAddress) {
    let marker = fixture.root.join("local-job.marker");
    let child = fixture.root.join("child-script.json");
    std::fs::write(&child, serde_json::to_vec(&json!({"version": 1, "steps": [
        {"output": {"tool": {"name": "bash", "arguments": {
            "command": format!("printf '%s %s\\n' \"$$\" \"$PPID\" >> '{}'; sleep 600", marker.display()), "timeout": 0
        }}}, "finish": "tool_calls"},
        {"stream": [{"gate": {"file": fixture.root.join("never")}}], "output": {"text": "child held"}}
    ]})).unwrap()).unwrap();
    let mut script = spawning_script();
    script["product_child_script"] = json!(child);
    script["steps"].as_array_mut().unwrap().truncate(1);
    script["steps"].as_array_mut().unwrap().push(if busy {
        json!({"stream": [{"gate": {"file": marker}}, {"text": PREFIX}, {"stall": 30000}]})
    } else {
        json!({"stream": [{"gate": {"file": marker}}], "output": {"text": ANSWER}})
    });
    fixture.write_script(&script);
    let host = start(fixture);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created")
    };
    let attachment = client.attach(2, &node);
    client.prompt(
        3,
        attachment,
        "delegate a process",
        if busy { PREFIX } else { ANSWER },
    );
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        use plakous_offline_crash_harness::evidence::{EvidenceKind, read_evidence};
        let dispatched = read_evidence(fixture.root.join("child-evidence.jsonl"))
            .unwrap_or_default()
            .iter()
            .filter(|event| event.kind == EvidenceKind::ProviderDispatched)
            .count();
        if dispatched >= 2 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the child never promoted its job"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    (host, session, node)
}

#[test]
fn host_crash_mid_child_stop_finishes_the_recorded_intent_once() {
    use std::os::unix::process::ExitStatusExt as _;
    for phase in ["before-stop", "after-stop"] {
        let Some(fixture) = fixture() else { return };
        let (mut host, session, node) = start_child_job(&fixture);
        let (pid, _) = job(&fixture);
        kill(&mut host);
        let mut host = start_env(
            &fixture,
            &[],
            &[("PLAK_TEST_HOST_READOPT_CUT", phase.into())],
        );
        let mut client = Client::connect(&host);
        client.attach(1, &node);
        client.send(&ClientFrame::Request {
            id: 2,
            op: Op::StopSession { session },
        });
        assert_eq!(cut_exit(host.child.as_mut().unwrap()).signal(), Some(9));
        host.child = None;
        for _ in 0..2 {
            let (mut child, _) = spawn_env(
                &fixture,
                &[],
                &[("PLAK_TEST_HOST_READOPT_CUT", phase.into())],
            );
            assert_eq!(cut_exit(&mut child).signal(), Some(9));
            if phase == "before-stop" {
                assert!(alive(pid));
            } else {
                stopped(pid);
            }
        }
        let mut host = start(&fixture);
        stopped(pid);
        job(&fixture);
        let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
        let connection = rusqlite::Connection::open(paths.runtime_database()).unwrap();
        let terminals: i64 = connection
            .query_row("SELECT count(*) FROM line_job_terminals", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(terminals, 1);
        stop(&mut host);
    }
}

#[test]
fn host_start_requests_use_each_hosts_captured_login_path() {
    use std::os::unix::fs::PermissionsExt as _;
    let Some(fixture) = fixture() else { return };
    for (index, path) in ["/first/bin:/usr/bin:/bin", "/second/bin:/usr/bin:/bin"]
        .into_iter()
        .enumerate()
    {
        let shell = fixture.root.join(format!("login-{index}"));
        std::fs::write(&shell, format!("#!/bin/sh\nprintf '%s' '{path}'\n")).unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        let marker = fixture.root.join(format!("environment-{index}"));
        let user_home = fixture.root.join(format!("user-home-{index}"));
        std::fs::create_dir(&user_home).unwrap();
        fixture.write_script(&json!({"version": 1, "process_tools": true, "steps": [
            {"output": {"tool": {"name": "process_session_start", "arguments": {
                "command": format!("printf '%s\\n%s' \"$PATH\" \"$HOME\" > '{}'", marker.display()), "workdir": ".",
                "login": false, "tty": false, "keep_stdin_open": false, "yield_time_ms": 1000,
                "max_output_bytes": 8192
            }}}, "finish": "tool_calls"}, {"output": {"text": ANSWER}, "finish": "stop"}
        ]}));
        let mut host = start_env(
            &fixture,
            &[],
            &[
                ("SHELL", shell.display().to_string()),
                ("HOME", user_home.display().to_string()),
            ],
        );
        let mut client = Client::connect(&host);
        let mut create = new_session(&fixture.workspace());
        if let Op::NewSession { request_id, .. } = &mut create {
            *request_id = format!("00000000-0000-4000-8000-{index:012}");
        }
        let Outcome::Ok(Answer::Created { node, .. }) = client.request(1, create) else {
            panic!("not created")
        };
        let attachment = client.attach(2, &node);
        client.prompt(3, attachment, "show the host path", ANSWER);
        assert_eq!(
            std::fs::read_to_string(marker).unwrap(),
            format!("{path}\n{}", user_home.display())
        );
        stop(&mut host);
    }
}

#[test]
fn host_crash_mid_readopt_converges_after_every_transfer_boundary() {
    use std::os::unix::process::ExitStatusExt as _;
    let Some(probe) = fixture() else { return };
    drop(probe);
    for phase in ["lease", "transfer", "terminals"] {
        let fixture = fixture().unwrap();
        let (mut host, session, node) = start_job(&fixture, false);
        let (pid, _) = job(&fixture);
        kill(&mut host);
        for _ in 0..2 {
            let (mut child, _) = spawn_env(
                &fixture,
                &[],
                &[("PLAK_TEST_HOST_READOPT_CUT", phase.into())],
            );
            let status = cut_exit(&mut child);
            assert_eq!(status.signal(), Some(9), "{phase}: {status}");
            assert!(alive(pid), "{phase}: the cut stopped the job");
        }
        let mut host = start(&fixture);
        let mut client = Client::connect(&host);
        client.attach(1, &node);
        assert!(alive(pid));
        assert_eq!(
            client.request(2, Op::StopSession { session }),
            Outcome::Ok(Answer::Stopped)
        );
        stopped(pid);
        job(&fixture);
        stop(&mut host);
    }
}

#[test]
fn host_crash_mid_stop_finishes_the_recorded_stop_once_after_retransfer() {
    use plak::store::session_records::SessionRecordStore as _;
    use plakous_hosted::sessions::projector::{RootInterrupt, RootLossIntent, RootStopAnchor};
    use std::os::unix::process::ExitStatusExt as _;

    let Some(probe) = fixture() else { return };
    drop(probe);
    for phase in ["before-stop", "after-stop"] {
        let fixture = fixture().unwrap();
        let (mut host, session, _) = start_job(&fixture, true);
        let (pid, _) = job(&fixture);
        kill(&mut host);
        let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
        let store = plak::store::session_records::SqliteSessionStore::new(paths.clone());
        // The durable side of the busy host's stop-before-cancel boundary.
        store
            .write_root_loss(
                &session,
                RootLossIntent::Interrupt(RootInterrupt {
                    anchor: RootStopAnchor::Command {
                        command_id: "plak:test:recorded-stop".into(),
                    },
                    cause: RootInterruptCause::Stopped,
                }),
            )
            .unwrap();
        for _ in 0..2 {
            let (mut child, _) = spawn_env(
                &fixture,
                &[],
                &[("PLAK_TEST_HOST_READOPT_CUT", phase.into())],
            );
            assert_eq!(cut_exit(&mut child).signal(), Some(9));
            if phase == "before-stop" {
                assert!(alive(pid));
            } else {
                stopped(pid);
            }
        }
        let mut host = start(&fixture);
        stopped(pid);
        job(&fixture);
        let connection = rusqlite::Connection::open(paths.runtime_database()).unwrap();
        let terminals: i64 = connection
            .query_row("SELECT count(*) FROM line_job_terminals", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(terminals, 1, "the terminal was lost or recorded twice");
        stop(&mut host);
        let mut host = start(&fixture);
        stopped(pid);
        assert_eq!(
            root_record(&fixture, &session).1,
            0,
            "a stopped turn continued"
        );
        stop(&mut host);
    }
}

#[test]
fn an_old_stop_never_stops_a_job_of_a_later_admitted_turn() {
    let Some(fixture) = fixture() else { return };
    let (mut host, session, node) = start_job(&fixture, true);
    let (old_pid, _) = job(&fixture);
    let mut client = Client::connect(&host);
    client.attach(1, &node);
    assert_eq!(
        client.request(
            2,
            Op::StopSession {
                session: session.clone()
            }
        ),
        Outcome::Ok(Answer::Stopped)
    );
    stopped(old_pid);
    assert_eq!(
        client.request(
            3,
            Op::ReleaseSession {
                session: session.clone()
            }
        ),
        Outcome::Ok(Answer::Released)
    );
    // Admit again on the same host: its startup-only keyed stop was never
    // spent, so only the later-admission guard can protect this new job.
    std::fs::remove_file(fixture.root.join("local-job.marker")).unwrap();
    let mut client = Client::connect(&host);
    let attachment = client.attach(1, &node);
    client.prompt(2, attachment, "start new work after the stop", PREFIX);
    let (pid, _) = job(&fixture);
    kill(&mut host);
    let mut host = start(&fixture);
    assert!(alive(pid), "an old stop killed a newer turn's job");
    let mut client = Client::connect(&host);
    client.attach(1, &node);
    assert_eq!(
        client.request(2, Op::StopSession { session }),
        Outcome::Ok(Answer::Stopped)
    );
    stopped(pid);
    stop(&mut host);
}
