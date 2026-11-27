//! Real transports observe planned notices before EOF, not inferred process loss.

use super::*;
use plakd_wire_types::lifecycle::{HostLifecycle, HostLifecyclePhase, HostLifecycleReason};
use std::io::Read as _;

fn observe(host: &Host) -> (BufReader<UnixStream>, u64) {
    let (reader, generation, _) = observe_binary(host);
    (reader, generation)
}

fn observe_binary(host: &Host) -> (BufReader<UnixStream>, u64, String) {
    let mut stream = UnixStream::connect(&host.socket).unwrap();
    crate::socket_timeout::allow_closed_peer(stream.set_read_timeout(Some(WAIT))).unwrap();
    stream
        .write_all(
            encode_line(&ClientFrame::<PlakExtension>::Hello(Hello {
                proto: WIRE_VERSION,
                binary_id: "lifecycle-test".into(),
                caps: Vec::new(),
            }))
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let HostFrame::Welcome(welcome) = decode_line::<HostFrame<PlakExtension>>(&line).unwrap()
    else {
        panic!("welcome")
    };
    assert_eq!(
        welcome.state_key,
        host.socket.file_stem().unwrap().to_str().unwrap()
    );
    (reader, welcome.host_generation, welcome.binary_id)
}

fn notices_to_eof(mut reader: BufReader<UnixStream>, generation: u64) -> Vec<HostLifecycle> {
    let mut notices = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        if let HostFrame::HostLifecycle {
            host_generation,
            phase,
            reason,
            target,
            retry_after_ms,
        } = match crate::protocol_13::decode(&line).unwrap() {
            Some(frame) => frame,
            None => continue,
        } {
            let notice = HostLifecycle {
                host_generation,
                phase,
                reason,
                target,
                retry_after_ms,
            };
            assert_eq!(notice.host_generation, generation);
            notices.push(notice);
        }
    }
    assert!(
        !notices.is_empty(),
        "transport closed without a planned notice"
    );
    notices
}

#[test]
fn host_lifecycle_protocol_twelve_gets_only_version_mismatch_even_at_planned_stop() {
    let Some(fixture) = fixture() else { return };
    let mut host = start(&fixture);
    let mut stream = UnixStream::connect(&host.socket).unwrap();
    crate::socket_timeout::allow_closed_peer(stream.set_read_timeout(Some(WAIT))).unwrap();
    stream
        .write_all(
            encode_line(&ClientFrame::<PlakExtension>::Hello(Hello {
                proto: 12,
                binary_id: "previous-protocol-test".into(),
                caps: Vec::new(),
            }))
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&line).unwrap(),
        json!({"frame":"refused","kind":"version_mismatch","host":WIRE_VERSION,"client":12})
    );
    stop(&mut host);
    let mut remaining = String::new();
    reader.read_to_string(&mut remaining).unwrap();
    assert!(
        remaining.is_empty(),
        "protocol 12 received new frames: {remaining}"
    );
}

#[test]
fn host_lifecycle_stalled_peer_uses_the_same_total_stop_budget_as_handoff() {
    let Some(fixture) = fixture() else { return };
    let budget = fixture.root.join("handoff-budget.json");
    let mut host = start_env(
        &fixture,
        &[],
        &[(
            "PLAK_TEST_HOST_HANDOFF_BUDGET",
            budget.display().to_string(),
        )],
    );
    let mut stalled = UnixStream::connect(&host.socket).unwrap();
    stalled.write_all(b"{\"frame\":\"hello\"").unwrap();
    let (_observer, _) = observe(&host);
    let began = Instant::now();
    let (stopped, finished) = mpsc::channel();
    std::thread::spawn(move || {
        stop(&mut host);
        stopped.send(()).unwrap();
    });
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let marker_deadline = Instant::now() + Duration::from_secs(1);
    while !std::fs::read_to_string(paths.host_lock())
        .unwrap()
        .contains(" stopping")
    {
        assert!(
            Instant::now() < marker_deadline,
            "stop marker was deferred until after the connection join"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let result = bridge::output_after_hello(
        fixture
            .command()
            .args(["host", "connect", "--stdio", "--auto-start"]),
        WIRE_VERSION,
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap(),
        json!({"frame":"refused","kind":"bridge_unavailable","reason":"stopping","retry_after_ms":500})
    );
    finished.recv_timeout(WAIT).unwrap();
    assert!(began.elapsed() < Duration::from_secs(50));
    let budget: serde_json::Value =
        serde_json::from_slice(&std::fs::read(budget).unwrap()).unwrap();
    let elapsed = budget["elapsed_ms"].as_u64().unwrap();
    let remaining = budget["remaining_ms"].as_u64().unwrap();
    assert!(
        elapsed >= 1900,
        "the stalled handshake did not exercise the join allowance: {budget}"
    );
    assert!(
        remaining < 44_000,
        "advisory waits were added outside the handoff budget: {budget}"
    );
    assert!(
        elapsed + remaining <= 45_000,
        "the stop margin was spent twice: {budget}"
    );
}

#[test]
fn host_lifecycle_sigterm_notice_precedes_transport_eof() {
    let Some(fixture) = fixture() else { return };
    let mut host = start(&fixture);
    let (reader, generation) = observe(&host);
    stop(&mut host);
    let notices = notices_to_eof(reader, generation);
    assert!(
        notices
            .iter()
            .all(|notice| notice.phase == HostLifecyclePhase::Stopping
                && notice.reason == HostLifecycleReason::Shutdown
                && notice.target.is_none())
    );
}

#[test]
fn host_lifecycle_restart_to_a_replacement_image_changes_generation_and_binary() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let image = identified_image(&fixture, &replacement_image(&fixture), 'b');
    let source = identified_image(&fixture, Path::new(env!("CARGO_BIN_EXE_plakd")), 'a');
    let mut host = start_image(&fixture, &source, &[]);
    let (reader, generation, binary) = observe_binary(&host);
    let mut successor = std::thread::scope(|scope| {
        let notices = scope.spawn(|| notices_to_eof(reader, generation));
        let successor = restart_to(&fixture, &mut host, &image, &[]);
        let notices = notices.join().unwrap();
        assert!(
            notices
                .iter()
                .all(|notice| notice.phase == HostLifecyclePhase::Stopping
                    && notice.reason == HostLifecycleReason::Shutdown
                    && notice.target.is_none())
        );
        successor
    });
    let (_, next_generation, next_binary) = observe_binary(&successor);
    assert_ne!(next_generation, generation);
    assert_eq!(binary, image_id(&fixture, &source));
    assert_eq!(next_binary, image_id(&fixture, &image));
    assert_ne!(
        next_binary, binary,
        "the successor is the replacement image"
    );
    stop(&mut successor);
}

#[test]
fn an_idle_stdio_mcp_child_is_reaped_by_the_planned_stop_and_the_successor_leases_a_fresh_one() {
    use rustix::process::{Pid, test_kill_process};
    use std::os::unix::fs::PermissionsExt as _;

    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let spawns = fixture.root.join("mcp-pids");
    let script = fixture.root.join("start-mcp");
    let server = std::env::var_os("PLAK_MCP_SERVER").expect("offline MCP server");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf '%s\\n' \"$$\" >> \"$1\"\nexec \"$2\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = fixture.home().join("config.toml");
    let body = std::fs::read_to_string(&config).unwrap();
    write_private(
        &config,
        &format!(
            "{body}\n[mcp_servers.fake]\ntransport = \"stdio\"\ncommand = {script:?}\nargs = [{spawns:?}, {server:?}]\n"
        ),
    );
    let lead = fixture.home().join("agents/lead.toml");
    let body = std::fs::read_to_string(&lead).unwrap();
    write_private(&lead, &format!("{body}\ntools = [\"mcp:fake\"]\n"));

    let mut host = start(&fixture);
    runner::wait_local_runner_ready(&host);
    let mut source = Client::connect(&host);
    let Outcome::Ok(Answer::Created { node, .. }) =
        source.request(1, new_session(&fixture.workspace()))
    else {
        panic!("MCP session creation refused");
    };
    source.attach(2, &node);
    drop(source);
    let pids = std::fs::read_to_string(&spawns).unwrap();
    let pid = Pid::from_raw(pids.trim().parse().unwrap()).unwrap();
    assert_eq!(test_kill_process(pid), Ok(()));

    let mut host = restart_to(&fixture, &mut host, &replacement_image(&fixture), &[]);
    assert_eq!(
        test_kill_process(pid),
        Err(rustix::io::Errno::SRCH),
        "the stopped host left its MCP child live or unreaped"
    );
    assert_eq!(
        std::fs::read_to_string(&spawns).unwrap(),
        pids,
        "startup restarted an idle server"
    );
    runner::wait_local_runner_ready(&host);
    Client::connect(&host).attach(2, &node);
    assert_eq!(
        std::fs::read_to_string(&spawns).unwrap().lines().count(),
        2,
        "the successor must lease a fresh server when the root opens"
    );
    stop(&mut host);
}

#[test]
fn a_restart_to_a_replacement_image_preserves_the_saved_session_and_queue() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let session = seed(&fixture, 83);
    let mut host = start(&fixture);
    let mut source = Client::connect(&host);
    let root = source
        .rows(1)
        .into_iter()
        .find(|row| row.session == session)
        .unwrap()
        .node;
    drop(source);
    let mut host = restart_to(&fixture, &mut host, &replacement_image(&fixture), &[]);
    let document = export(&fixture, &session, "restart-export.json");
    assert_eq!(document["queue"], json!(["paused input 83"]));
    assert_eq!(document["named_state"], json!({"answer":83}));
    Client::connect(&host).attach(2, &root);
    stop(&mut host);
}

#[test]
fn sigkill_during_a_stalled_stop_recovers_once_with_the_host_default() {
    let Some(fixture) = fixture() else { return };
    let fixture = fixture.with_session_lease_seconds(2);
    let _cleanup = HomeHost(fixture.home());
    fixture.write_script(&json!({
        "version":1,
        "steps":[{"stream":[{"text":"source dispatch"},{"gate":{"file":fixture.root.join("never-release")}}]}]
    }));
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created");
    };
    let attachment = client.attach(2, &node);
    let accepted = client.request(
        3,
        Op::Intent {
            attachment,
            intent: SessionIntent::Prompt(Draft::from("recover this turn once")),
            client_id: Some(plakous_host_protocol::wire::ClientDraftId::new()),
            blobs: Vec::new(),
        },
    );
    assert!(matches!(accepted, Outcome::Ok(Answer::Accepted { .. })));
    let deadline = Instant::now() + WAIT;
    while !fixture.root.join("evidence.jsonl").is_file() || dispatches(&fixture) < 1 {
        assert!(Instant::now() < deadline, "root did not dispatch");
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut stalled = UnixStream::connect(&host.socket).unwrap();
    stalled.write_all(b"{\"frame\":\"hello\"").unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(host.child.as_ref().unwrap().id().cast_signed()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    while !std::fs::read_to_string(paths.host_lock())
        .unwrap()
        .contains(" stopping")
    {
        assert!(
            Instant::now() < deadline,
            "stop did not reach the connection join"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    kill(&mut host);
    drop(stalled);
    drop(client);

    fixture.write_script(&json!({
        "version":1, "repeat_last_step":true, "steps":[{"output":{"text":ANSWER}}]
    }));
    let image = replacement_image(&fixture);
    let mut host = start_image(&fixture, &image, &[]);
    // A crash before planned release has no handoff row for the startup scan.
    // Opening the session proves ownership loss and applies the host default.
    let mut client = Client::connect(&host);
    client.attach(4, &node);
    let deadline = Instant::now() + WAIT;
    while root_record(&fixture, &session).1 == 0 {
        assert!(
            Instant::now() < deadline,
            "unset host policy did not continue the lost turn"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(client);
    stop(&mut host);
    assert_eq!(root_record(&fixture, &session).1, 1);
    assert_eq!(dispatches(&fixture), 2);
}
