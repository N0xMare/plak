//! Re-exec at the real ownership and listening-socket boundaries.

use super::*;
use std::fs::{File, OpenOptions, TryLockError};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::Path;
use std::time::{Duration, Instant};

fn identify(command: &mut std::process::Command) -> String {
    let probe = command
        .args(["version", "--host-upgrade-probe"])
        .output()
        .unwrap();
    assert!(probe.status.success());
    let probe: Value = serde_json::from_slice(&probe.stdout).unwrap();
    probe["binary_id"].as_str().unwrap().to_owned()
}

fn read_control(reader: &mut BufReader<UnixStream>) -> Value {
    let mut line = String::new();
    assert_ne!(reader.read_line(&mut line).unwrap(), 0);
    serde_json::from_str(&line).unwrap()
}

fn coordinator(fixture: &Fixture) -> BufReader<UnixStream> {
    let stream = UnixStream::connect(fixture.home().join("host/upgrade.sock")).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    BufReader::new(stream)
}

// Stop at the source's Ready; the CLI would wait for an intentionally
// unavailable destination binary. Test the successor's Welcome directly.
fn prepare(fixture: &Fixture, candidate: &Path, binary: &str) -> BufReader<UnixStream> {
    let mut control = coordinator(fixture);
    let identity = read_control(&mut control);
    writeln!(
        control.get_mut(),
        "{}",
        json!({
            "source_instance": identity["instance"], "source_key": identity["key"],
            "destination": candidate, "destination_key": identity["key"],
            "expected_binary": binary, "wait_seconds": 0,
            "drain": {"mode": "bounded", "milliseconds": 0}, "abandon": null
        })
    )
    .unwrap();
    control
}

fn prepare_answer(control: &mut BufReader<UnixStream>) -> Value {
    let deadline = Instant::now() + WAIT;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .expect("upgrade prepare did not answer");
        control.get_mut().set_read_timeout(Some(remaining)).unwrap();
        let frame = read_control(control);
        if frame["status"] != "progress" {
            return frame;
        }
    }
}

fn ready(control: &mut BufReader<UnixStream>) {
    let answer = prepare_answer(control);
    assert_eq!(answer["status"], "ready", "{answer}");
}

fn wait_line(web: &Web, expected: &str) {
    let deadline = Instant::now() + WAIT;
    while let Ok(line) = web
        .lines
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
    {
        if line.contains(expected) {
            return;
        }
    }
    panic!("host did not report {expected}");
}

fn wait_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "host did not exit");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn start_gated(fixture: &Fixture, gate: &Path) -> Web {
    std::fs::write(gate, []).unwrap();
    let (web, started) = launch_on_with_env(
        fixture,
        &socket_dir(fixture).join("plak.sock"),
        &[],
        &[("PLAK_TEST_HOST_BEFORE_PAIRED_EXEC", gate.to_str().unwrap())],
        CSP,
    );
    assert!(started, "{:?}", web.lines.try_iter().collect::<Vec<_>>());
    web
}

fn locks(fixture: &Fixture) -> [File; 2] {
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    [fixture.home().join("host/host.lock"), paths.host_lock()].map(|path| {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap()
    })
}

fn inode(path: &Path) -> u64 {
    std::fs::symlink_metadata(path).unwrap().ino()
}

fn config_get(socket: &mut WebSocket<UnixStream>) -> Value {
    socket
        .send(Message::text(
            json!({
                "frame": "request", "id": 1,
                "op": {"op": "host", "request": {"request": "config_get"}}
            })
            .to_string(),
        ))
        .unwrap();
    response(socket, 1)
}

fn assert_web_console(welcome: &Value, enabled: bool) {
    let console = &welcome["extension"]["console"];
    assert_eq!(console["config_read"]["enabled"], enabled, "{welcome}");
    assert_eq!(console["terminal"]["enabled"], enabled, "{welcome}");
    assert_eq!(console["accounts_read"]["enabled"], true, "{welcome}");
}

fn assert_waiting_for_scan(fixture: &Fixture, socket: &mut WebSocket<UnixStream>) {
    send(
        socket,
        &ClientFrame::Request {
            id: 2,
            op: Op::NewSession {
                runner: plakous_host_protocol::wire::RunnerLabel::local(),
                profile: None,
                model: None,
                workspace: fixture.workspace().display().to_string(),
                template: None,
                request_id: REQUEST.into(),
                accounts: Vec::new(),
            },
        },
    );
    assert_eq!(
        response(socket, 2),
        json!({"refused":{"refusal":"backpressure"}})
    );
}

fn assert_login_revoked(web: &Web, login: &str, cookie: &str) {
    let mut headers = upgrade_headers(Some(ORIGIN), Some(cookie));
    headers
        .iter_mut()
        .find(|(name, _)| *name == "Tailscale-User-Login")
        .unwrap()
        .1 = login.into();
    assert_eq!(
        exchange(&mut connect(web), "GET", "/wire", &headers).status,
        403
    );
}

#[test]
fn successor_serves_the_new_home_web_policy_on_the_inherited_listener() {
    const REMOVED_LOGIN: &str = "revoked@example.com";
    let fixture = Fixture::new(&json!({"version":1,"steps":[]}));
    let write_policy = |allow: &str, enabled: bool| {
        fixture.write_config(&format!(
            "[web]\nallow = [{allow}]\norigin = \"{ORIGIN}\"\naccounts_read = true\nconfig_read = {enabled}\nterminal = {enabled}\n"
        ));
    };
    write_policy(&format!("\"{LOGIN}\", \"{REMOVED_LOGIN}\""), true);
    let source_csp =
        latency::CONSOLE_CSP.replace("script-src 'self'", "script-src 'self' 'wasm-unsafe-eval'");
    let exec_gate = fixture.root.join("exec-gate");
    let scan_gate = fixture.root.join("scan-gate");
    std::fs::write(&exec_gate, []).unwrap();
    let (mut web, started) = launch_on_with_env(
        &fixture,
        &socket_dir(&fixture).join("plak.sock"),
        &[],
        &[
            (
                "PLAK_TEST_HOST_BEFORE_PAIRED_EXEC",
                exec_gate.to_str().unwrap(),
            ),
            (
                "PLAK_TEST_HOST_CONTINUATION_SCAN_GATE",
                scan_gate.to_str().unwrap(),
            ),
        ],
        &source_csp,
    );
    assert!(started, "{:?}", web.lines.try_iter().collect::<Vec<_>>());
    let listener_inode = inode(&web.socket);
    let old_page = page(&web, Some(REMOVED_LOGIN), HOST);
    assert_eq!(old_page.status, 200);
    assert_eq!(
        old_page.header("content-security-policy"),
        Some(source_csp.as_str())
    );
    let revoked_cookie = old_page
        .header("set-cookie")
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    let retained_cookie = cookie(&web);
    let mut source = upgrade(&web, &retained_cookie);
    let old = directory(&mut source);
    assert_web_console(&old, true);
    assert_eq!(
        config_get(&mut source)["ok"]["response"]["response"],
        "config_get"
    );

    let candidate = replacement_image(&fixture).canonicalize().unwrap();
    let binary = identify(&mut image_command(&fixture, &candidate));
    assert_ne!(old["binary_id"], binary);
    let mut control = prepare(&fixture, &candidate, &binary);
    ready(&mut control);
    wait_line(&web, "waiting at paired exec gate");
    write_policy(&format!("\"{LOGIN}\""), false);
    std::fs::write(
        fixture.root.join("ui/csp.json"),
        json!({"header":latency::CONSOLE_CSP}).to_string(),
    )
    .unwrap();
    std::fs::write(&scan_gate, []).unwrap();
    std::fs::remove_file(exec_gate).unwrap();

    let mut successor = upgrade(&web, &retained_cookie);
    let welcome = directory(&mut successor);
    assert_ne!(welcome["host_generation"], old["host_generation"]);
    assert_eq!(welcome["binary_id"], binary);
    assert_eq!(inode(&web.socket), listener_inode);
    assert_waiting_for_scan(&fixture, &mut successor);
    assert_web_console(&welcome, false);
    let outcome = config_get(&mut successor);
    assert_eq!(
        outcome["refused"],
        json!({"refusal": "capability_disabled", "capability": "config_read"}),
        "{outcome}"
    );
    let new_page = page(&web, Some(LOGIN), HOST);
    assert_eq!(new_page.status, 200);
    assert_eq!(
        new_page.header("content-security-policy"),
        Some(latency::CONSOLE_CSP)
    );
    assert_login_revoked(&web, REMOVED_LOGIN, revoked_cookie);

    std::fs::remove_file(scan_gate).unwrap();
    drop(successor);
    interrupt_and_wait(&mut web);
}

#[test]
fn changed_image_rolls_back_with_the_same_pid_and_listeners() {
    let fixture = Fixture::new(&json!({"version":1,"steps":[]}));
    let binary = identify(&mut fixture.command());
    let candidate = replacement_image(&fixture).canonicalize().unwrap();
    let destination = identify(&mut image_command(&fixture, &candidate));
    assert_ne!(binary, destination);
    let gate = fixture.root.join("exec-gate");
    let mut web = start_gated(&fixture, &gate);
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let control_path = paths.host_socket();
    let sockets = [inode(&control_path), inode(&web.socket)];
    let pid = web.child.as_ref().unwrap().id().to_string();
    let login = cookie(&web);
    let mut source = upgrade(&web, &login);
    let old = directory(&mut source);
    let mut control = prepare(&fixture, &candidate, &destination);
    ready(&mut control);
    wait_line(&web, "waiting at paired exec gate");
    OpenOptions::new()
        .append(true)
        .open(candidate)
        .unwrap()
        .write_all(b"changed")
        .unwrap();

    for lock in locks(&fixture) {
        assert!(matches!(lock.try_lock(), Err(TryLockError::WouldBlock)));
    }
    let mut rival = Child::spawn(fixture.command().arg("host").stderr(Stdio::piped()));
    assert!(!wait_exit(&mut rival).success());
    let mut refusal = String::new();
    rival
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut refusal)
        .unwrap();
    assert!(refusal.contains(&format!("holder {pid}")), "{refusal}");

    std::fs::remove_file(gate).unwrap();
    wait_line(&web, "qualified image changed");
    let mut successor = upgrade(&web, &login);
    let restored = directory(&mut successor);
    assert_ne!(restored["host_generation"], old["host_generation"]);
    assert_eq!(restored["binary_id"], binary);
    assert_eq!([inode(&control_path), inode(&web.socket)], sockets);
    assert!(web.child.as_mut().unwrap().try_wait().unwrap().is_none());
    assert_eq!(
        std::fs::read_to_string(paths.host_lock()).unwrap().trim(),
        format!("{pid} {binary}")
    );
    assert_eq!(read_control(&mut coordinator(&fixture))["binary"], binary);
    let _new_session = create(&mut successor);
    interrupt_and_wait(&mut web);
}

#[test]
fn destination_without_handoff_capability_is_refused_before_pause() {
    let fixture = Fixture::new(&json!({"version":1,"steps":[]}));
    let (mut web, started) = launch_on_with_env(
        &fixture,
        &socket_dir(&fixture).join("plak.sock"),
        &[],
        &[("PLAK_TEST_HOST_UPGRADE_OMIT_REEXEC", "1")],
        CSP,
    );
    assert!(started);
    let login = cookie(&web);
    let mut client = upgrade(&web, &login);
    let old = directory(&mut client);
    let candidate = Path::new(env!("CARGO_BIN_EXE_plakd"))
        .canonicalize()
        .unwrap();
    let answer = prepare_answer(&mut prepare(
        &fixture,
        &candidate,
        &identify(&mut fixture.command()),
    ));
    assert_eq!(answer["status"], "refused", "{answer}");
    assert!(
        answer["detail"]
            .as_str()
            .unwrap()
            .starts_with("host_upgrade_destination_unsupported"),
        "{answer}"
    );
    send(
        &mut client,
        &ClientFrame::Request {
            id: 1,
            op: Op::Ping,
        },
    );
    let pong = next_frame(&mut client);
    assert_eq!(pong["outcome"]["ok"]["answer"], "pong", "{pong}");
    assert_eq!(
        directory(&mut upgrade(&web, &login))["host_generation"],
        old["host_generation"]
    );
    let _new_session = create(&mut client);
    interrupt_and_wait(&mut web);
}

#[test]
fn successor_with_a_bad_socket_releases_both_locks_for_cold_start() {
    let fixture = Fixture::new(&json!({"version":1,"steps":[]}));
    let gate = fixture.root.join("exec-gate");
    let mut web = start_gated(&fixture, &gate);
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let candidate = Path::new(env!("CARGO_BIN_EXE_plakd"))
        .canonicalize()
        .unwrap();
    let mut control = prepare(&fixture, &candidate, &identify(&mut fixture.command()));
    ready(&mut control);
    wait_line(&web, "waiting at paired exec gate");
    std::fs::set_permissions(paths.host_socket(), std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::remove_file(gate).unwrap();

    wait_line(&web, "cannot adopt the inherited host control socket");
    assert!(!wait_exit(&mut web.child.take().unwrap()).success());
    for lock in locks(&fixture) {
        lock.try_lock()
            .expect("successor released the ownership lock");
    }
    let mut cold = start(&fixture);
    let mut client = upgrade(&cold, &cookie(&cold));
    directory(&mut client);
    let _new_session = create(&mut client);
    interrupt_and_wait(&mut cold);
}
