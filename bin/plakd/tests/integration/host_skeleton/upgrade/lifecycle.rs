//! Real transports observe planned notices before EOF, not inferred process loss.

use super::*;
use plakd_wire_types::lifecycle::{HostLifecycle, HostLifecyclePhase, HostLifecycleReason};
use std::io::Read as _;

fn observe(host: &Host) -> (BufReader<UnixStream>, u64) {
    let mut stream = UnixStream::connect(&host.socket).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
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
    (reader, welcome.host_generation)
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
        } = decode_line::<HostFrame<PlakExtension>>(&line).unwrap()
        {
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
fn host_lifecycle_wire_ten_gets_only_version_mismatch_even_at_planned_stop() {
    let Some(fixture) = fixture() else { return };
    let mut host = start(&fixture);
    let mut stream = UnixStream::connect(&host.socket).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream
        .write_all(
            encode_line(&ClientFrame::<PlakExtension>::Hello(Hello {
                proto: 10,
                binary_id: "wire-ten-test".into(),
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
        json!({"frame":"refused","kind":"version_mismatch","host":WIRE_VERSION,"client":10})
    );
    stop(&mut host);
    let mut remaining = String::new();
    reader.read_to_string(&mut remaining).unwrap();
    assert!(
        remaining.is_empty(),
        "wire 10 received new frames: {remaining}"
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
fn host_lifecycle_same_key_restart_precedes_eof_and_changes_generation() {
    use std::os::unix::fs::MetadataExt as _;
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let order = fixture.root.join("ready-order");
    let mut host = start_env(
        &fixture,
        &[],
        &[(
            "PLAK_TEST_HOST_UPGRADE_READY_ORDER",
            order.display().to_string(),
        )],
    );
    let mut stalled = UnixStream::connect(&host.socket).unwrap();
    let inode = std::fs::symlink_metadata(&host.socket).unwrap().ino();
    stalled.write_all(b"{\"frame\":\"hello\"").unwrap();
    let (reader, generation) = observe(&host);
    let id = binary(&fixture);
    let mut request = fixture.command();
    request
        .args(["host", "upgrade", "--expect-binary", &id, "--drain", "0"])
        .env("PLAK_TEST_FORCE_HOST_UPGRADE", "1");
    let upgrading = std::thread::spawn(move || request.output().unwrap());
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let deadline = Instant::now() + WAIT;
    while !std::fs::read_to_string(paths.host_lock())
        .unwrap_or_default()
        .contains(" restarting")
    {
        assert!(
            Instant::now() < deadline,
            "committed restart was not marked during the stalled-peer join"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let reconnected = bridge::output_after_hello(
        fixture
            .command()
            .args(["host", "connect", "--stdio", "--auto-start"]),
        WIRE_VERSION,
    );
    assert!(
        reconnected.status.success(),
        "{}",
        String::from_utf8_lossy(&reconnected.stderr)
    );
    let welcome: serde_json::Value = serde_json::from_slice(&reconnected.stdout).unwrap();
    assert_eq!(
        welcome["frame"], "welcome",
        "a committed restart must be awaited, not refused"
    );
    assert_ne!(welcome["host_generation"], generation);
    assert_eq!(
        std::fs::symlink_metadata(&host.socket).unwrap().ino(),
        inode
    );
    let result = upgrading.join().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(order).unwrap(),
        "restarting",
        "ready became observable before restarting was published"
    );
    let notices = notices_to_eof(reader, generation);
    assert_eq!(notices.first().unwrap().phase, HostLifecyclePhase::Draining);
    assert_eq!(
        notices.last().unwrap().phase,
        HostLifecyclePhase::Restarting
    );
    assert!(
        notices
            .iter()
            .all(|notice| notice.reason == HostLifecycleReason::PairedUpgrade
                && notice.target.as_ref().unwrap().binary_id == id)
    );
    let (_, next_generation) = observe(&host);
    assert_ne!(next_generation, generation);
    stop(&mut host);
}

#[test]
fn an_idle_stdio_mcp_child_is_reaped_before_same_key_exec() {
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
    let (reader, generation) = observe(&host);
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

    let result = fixture
        .command()
        .args([
            "host",
            "upgrade",
            "--expect-binary",
            &binary(&fixture),
            "--drain",
            "0",
        ])
        .env("PLAK_TEST_FORCE_HOST_UPGRADE", "1")
        .output()
        .unwrap();
    require_upgrade(&fixture, &host, "stdio MCP child reap", &result);
    notices_to_eof(reader, generation);
    let (_, next_generation) = observe(&host);
    assert_ne!(next_generation, generation);
    assert_eq!(
        test_kill_process(pid),
        Err(rustix::io::Errno::SRCH),
        "source MCP child remained live or unreaped after exec"
    );
    assert_eq!(
        std::fs::read_to_string(&spawns).unwrap(),
        pids,
        "qualification or startup restarted an idle server"
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
fn host_lifecycle_same_key_paired_upgrade_names_the_new_binary_before_eof() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let image = replacement_image(&fixture);
    let probe = image_command(&fixture, &image)
        .args(["version", "--host-upgrade-probe"])
        .output()
        .unwrap();
    assert!(probe.status.success());
    let version: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
    let id = version["binary_id"].as_str().unwrap();
    assert_ne!(id, binary(&fixture));
    let mut host = start(&fixture);
    let (reader, generation) = observe(&host);
    let result = image_command(&fixture, &image)
        .args(["host", "upgrade", "--expect-binary", id, "--drain", "0"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let notices = notices_to_eof(reader, generation);
    assert_eq!(notices.first().unwrap().phase, HostLifecyclePhase::Draining);
    assert_eq!(
        notices.last().unwrap().phase,
        HostLifecyclePhase::Restarting
    );
    assert!(
        notices
            .iter()
            .all(|notice| notice.reason == HostLifecycleReason::PairedUpgrade
                && notice.target.as_ref().unwrap().binary_id == id)
    );
    stop(&mut host);
}

#[test]
fn host_lifecycle_same_key_upgrade_preserves_the_saved_session_and_queue() {
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
    let (reader, generation) = observe(&host);
    let image = replacement_image(&fixture);
    let probe = image_command(&fixture, &image)
        .args(["version", "--host-upgrade-probe"])
        .output()
        .unwrap();
    assert!(probe.status.success());
    let version: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
    let id = version["binary_id"].as_str().unwrap();
    assert_ne!(id, binary(&fixture));
    let result = image_command(&fixture, &image)
        .args(["host", "upgrade", "--expect-binary", id, "--drain", "0"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let notices = notices_to_eof(reader, generation);
    let restarting = notices.last().unwrap();
    assert_eq!(restarting.phase, HostLifecyclePhase::Restarting);
    assert_eq!(restarting.reason, HostLifecycleReason::PairedUpgrade);
    assert_eq!(restarting.target.as_ref().unwrap().binary_id, id);
    let (_, next_generation) = observe(&host);
    assert_ne!(next_generation, generation);
    let document = export(&fixture, &session, false, "handoff-export.json");
    assert_eq!(document["queue"], json!(["paused input 83"]));
    assert_eq!(document["named_state"], json!({"answer":83}));
    Client::connect(&host).attach(2, &root);
    stop(&mut host);
}
