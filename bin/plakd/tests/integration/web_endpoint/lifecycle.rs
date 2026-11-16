//! Real web transports receive planned lifecycle before their connection closes.

use super::*;

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
    let fixture = Fixture::new(&json!({"version":1,"steps":[]}));
    let mut web = start(&fixture);
    let mut socket = upgrade(&web, &cookie(&web));
    let welcome = directory(&mut socket);
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
    let notices = notices_until_close(&mut socket, &welcome["host_generation"]);
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
    interrupt_and_wait(&mut web);
}
