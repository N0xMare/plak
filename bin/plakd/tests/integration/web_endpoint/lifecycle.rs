//! Real web transports receive planned lifecycle before their connection closes.

use super::*;

mod handoff;

fn directory(socket: &mut WebSocket<UnixStream>) -> Value {
    send(
        socket,
        &ClientFrame::Hello(Hello {
            proto: WIRE_VERSION,
            binary_id: "web-lifecycle-test".into(),
            caps: Vec::new(),
        }),
    );
    let welcome = next_frame(socket);
    assert_eq!(welcome["frame"], "welcome");
    assert!(welcome.get("session").is_none());
    welcome
}

fn notices_until_close(socket: &mut WebSocket<UnixStream>, generation: &Value) -> Vec<Value> {
    let mut notices = Vec::new();
    loop {
        match socket.read() {
            Ok(Message::Text(text)) => {
                let frame: Value = serde_json::from_str(text.as_str()).unwrap();
                if frame["frame"] == "host_lifecycle" {
                    assert_eq!(&frame["host_generation"], generation);
                    notices.push(frame);
                }
            }
            Ok(Message::Close(_))
            | Err(
                tungstenite::Error::ConnectionClosed
                | tungstenite::Error::AlreadyClosed
                | tungstenite::Error::Protocol(
                    tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
                ),
            ) => return notices,
            // The host sent its close frame and exited, so the close reply
            // had nowhere to go: the connection closed.
            Err(tungstenite::Error::Io(error))
                if error.kind() == std::io::ErrorKind::BrokenPipe =>
            {
                return notices;
            }
            Ok(Message::Ping(_) | Message::Pong(_)) => {}
            other => panic!("unexpected web lifecycle result: {other:?}"),
        }
    }
}

#[test]
fn web_lifecycle_sigterm_delivers_stopping_shutdown_before_close() {
    let fixture = Fixture::new(&json!({"version":1,"steps":[]}));
    let mut web = start(&fixture);
    let mut socket = upgrade(&web, &cookie(&web));
    let welcome = directory(&mut socket);
    let pid = web.child.as_ref().unwrap().id().to_string();
    assert!(
        std::process::Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success()
    );
    let notices = notices_until_close(&mut socket, &welcome["host_generation"]);
    assert!(
        !notices.is_empty(),
        "WebSocket closed without its planned-stop notice"
    );
    assert!(
        notices
            .iter()
            .all(|notice| notice["phase"] == "stopping" && notice["reason"] == "shutdown")
    );
    assert!(web.child.take().unwrap().wait().unwrap().success());
}

#[test]
fn web_lifecycle_same_key_upgrade_delivers_draining_then_restarting() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let fixture = Fixture::new(&json!({"version":1,"steps":[]}));
    let shell = fixture.root.join("login-shell");
    let invoked = fixture.home().join("shell-invocations");
    std::fs::write(
        &shell,
        "#!/bin/sh\necho \"$PPID\" >> \"$PLAK_HOME/shell-invocations\"\nprintf /usr/bin:/bin\n",
    )
    .unwrap();
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (mut web, ready) = launch_on_with_env(
        &fixture,
        &socket_dir(&fixture).join("plak.sock"),
        &[],
        &[("SHELL", shell.to_str().unwrap())],
        CSP,
    );
    assert!(ready);
    let pid = web.child.as_ref().unwrap().id().to_string();
    let login = cookie(&web);
    let inode = std::fs::symlink_metadata(&web.socket).unwrap().ino();
    let mut socket = upgrade(&web, &login);
    let welcome = directory(&mut socket);
    // Read while the upgrade runs: frames left unread until the host has
    // exited cannot be reached once a pending reply fails to write.
    let generation = welcome["host_generation"].clone();
    let reader = std::thread::spawn(move || notices_until_close(&mut socket, &generation));
    let probe = fixture
        .command()
        .args(["version", "--host-upgrade-probe"])
        .output()
        .unwrap();
    let probe: Value = serde_json::from_slice(&probe.stdout).unwrap();
    let result = fixture
        .command()
        .args([
            "host",
            "upgrade",
            "--expect-binary",
            probe["binary_id"].as_str().unwrap(),
            "--drain",
            "0",
        ])
        .env("PLAK_TEST_FORCE_HOST_UPGRADE", "1")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let notices = reader.join().unwrap();
    assert_eq!(
        notices
            .first()
            .map(|notice| notice["phase"].as_str().unwrap()),
        Some("draining")
    );
    assert_eq!(
        notices
            .last()
            .map(|notice| notice["phase"].as_str().unwrap()),
        Some("restarting")
    );
    assert!(
        notices
            .iter()
            .all(|notice| notice["reason"] == "paired_upgrade"
                && notice["target"]["state_key"] == welcome["state_key"])
    );
    assert_eq!(std::fs::symlink_metadata(&web.socket).unwrap().ino(), inode);
    let mut successor = upgrade(&web, &login);
    assert_ne!(
        directory(&mut successor)["host_generation"],
        welcome["host_generation"]
    );
    assert_eq!(
        std::fs::read_to_string(invoked)
            .unwrap()
            .lines()
            .filter(|line| *line == pid)
            .count(),
        1
    );
    drop(successor);
    interrupt_and_wait(&mut web);
}

#[test]
fn web_welcome_and_console_reads_do_not_wait_for_owed_sessions() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "steps": [{"output": {"text": "continued"}}]
    }));
    let config = fixture.home().join("config.toml");
    let original = std::fs::read_to_string(&config).unwrap();
    write_private(
        &config,
        &format!(
            "{original}\n[web]\nallow = [\"{LOGIN}\"]\norigin = \"{ORIGIN}\"\nconfig_read = true\n"
        ),
    );
    let gate = fixture.root.join("scan-gate");
    let release = fixture.root.join("provider-release");
    let (mut web, ready) = launch_on_with_env(
        &fixture,
        &socket_dir(&fixture).join("plak.sock"),
        &[],
        &[
            (
                "PLAK_TEST_HOST_CONTINUATION_SCAN_GATE",
                gate.to_str().unwrap(),
            ),
            ("PLAK_TEST_PROVIDER_RELEASE", release.to_str().unwrap()),
        ],
        latency::CONSOLE_CSP,
    );
    assert!(ready, "{:?}", web.lines.try_iter().collect::<Vec<_>>());
    let login = cookie(&web);
    let mut source = upgrade(&web, &login);
    let old = directory(&mut source);
    let root = start_owed_turn(&fixture, &mut source);
    let probe = fixture
        .command()
        .args(["version", "--host-upgrade-probe"])
        .output()
        .unwrap();
    let probe: Value = serde_json::from_slice(&probe.stdout).unwrap();
    std::fs::write(&gate, []).unwrap();
    let mut requester = Child::spawn(
        fixture
            .command()
            .args([
                "host",
                "upgrade",
                "--expect-binary",
                probe["binary_id"].as_str().unwrap(),
                "--drain",
                "0",
            ])
            .env("PLAK_TEST_FORCE_HOST_UPGRADE", "1")
            .stdout(Stdio::null()),
    );
    let _notices = notices_until_close(&mut source, &old["host_generation"]);
    let mut socket = upgrade(&web, &login);
    let successor = directory(&mut socket);
    assert_ne!(successor["host_generation"], old["host_generation"]);
    let starting = next_frame(&mut socket);
    assert_eq!(starting["frame"], "host_lifecycle", "{starting}");
    assert_eq!(starting["host_generation"], successor["host_generation"]);
    assert_eq!(starting["phase"], "starting");
    assert_eq!(starting["reason"], "paired_upgrade");
    assert!(matches!(
        starting["seconds_remaining"].as_u64(),
        Some(1..=90)
    ));
    assert_owed_directory(&fixture, &mut socket, &root);
    socket.send(Message::text(json!({
        "frame": "request", "id": 2, "op": {"op": "host", "request": {"request": "config_get"}}
    }).to_string())).unwrap();
    assert!(response(&mut socket, 2).get("ok").is_some());
    send(
        &mut socket,
        &ClientFrame::Request {
            id: 3,
            op: Op::Attach { node: root.clone() },
        },
    );
    assert_eq!(
        response(&mut socket, 3),
        json!({"refused":{"refusal":"backpressure"}})
    );
    std::fs::remove_file(gate).unwrap();
    std::fs::write(release, []).unwrap();
    assert!(requester.wait().unwrap().success());
    let running = next_frame(&mut socket);
    assert_eq!(running["frame"], "host_lifecycle", "{running}");
    assert_eq!(running["host_generation"], successor["host_generation"]);
    assert_eq!(running["phase"], "running");
    assert_eq!(running["reason"], "paired_upgrade");
    assert_eq!(running["seconds_remaining"], 0);
    attach(&mut socket, &root, 4);
    drop(socket);
    interrupt_and_wait(&mut web);
}

fn start_owed_turn(fixture: &Fixture, socket: &mut WebSocket<UnixStream>) -> NodeAddress {
    let root = create(socket);
    let attachment = attach(socket, &root, 1);
    send(
        socket,
        &ClientFrame::Request {
            id: 2,
            op: Op::Intent {
                attachment,
                intent: SessionIntent::Prompt(Draft::from("keep this turn across the handoff")),
                client_id: Some(uuid::Uuid::new_v4().to_string().parse().unwrap()),
                blobs: Vec::new(),
            },
        },
    );
    assert_eq!(response(socket, 2)["ok"]["answer"], "accepted");
    wait_for_provider_dispatch(fixture);
    root
}

fn assert_owed_directory(
    fixture: &Fixture,
    socket: &mut WebSocket<UnixStream>,
    root: &NodeAddress,
) {
    send(
        socket,
        &ClientFrame::Request {
            id: 1,
            op: Op::ListSessions {
                workspace: None,
                runner: None,
                include_archived: false,
            },
        },
    );
    let rows = response(socket, 1);
    assert_eq!(rows["ok"]["answer"], "sessions", "{rows}");
    let rows = rows["ok"]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["node"], serde_json::to_value(root).unwrap());
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let store = plakd::store::session_records::SqliteSessionStore::new(paths);
    let session = rows[0]["session"].as_str().unwrap();
    assert!(matches!(
        plakous_hosted::runtime::root_standing(&store, session).unwrap(),
        plakous_hosted::runtime::RootStanding::Continuing { handoff: true, .. }
    ));
}
