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

/// A restart upgrade is a planned stop and a start on the same stores: the
/// successor binds the same web socket path under a new generation, lists
/// the handed-off root as continuing, and admits an attach to it.
#[test]
fn web_restart_serves_a_new_generation_with_the_handed_off_root_continuing() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "steps": [{"output": {"text": "continued"}}]
    }));
    let release = fixture.root.join("provider-release");
    let socket_path = socket_dir(&fixture).join("plak.sock");
    let environment = [("PLAK_TEST_PROVIDER_RELEASE", release.to_str().unwrap())];
    let (mut web, ready) = launch_on_with_env(
        &fixture,
        &socket_path,
        &[],
        &environment,
        latency::CONSOLE_CSP,
    );
    assert!(ready, "{:?}", web.lines.try_iter().collect::<Vec<_>>());
    let login = cookie(&web);
    let mut source = upgrade(&web, &login);
    let old = directory(&mut source);
    let root = start_owed_turn(&fixture, &mut source);
    let pid = web.child.as_ref().unwrap().id().to_string();
    assert!(
        std::process::Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success()
    );
    let notices = notices_until_close(&mut source, &old["host_generation"]);
    assert!(notices.iter().all(|notice| notice["phase"] == "stopping"));
    assert!(web.child.take().unwrap().wait().unwrap().success());
    std::fs::write(&release, []).unwrap();
    let (mut web, ready) = launch_on_with_env(
        &fixture,
        &socket_path,
        &[],
        &environment,
        latency::CONSOLE_CSP,
    );
    assert!(ready, "{:?}", web.lines.try_iter().collect::<Vec<_>>());
    let mut socket = upgrade(&web, &login);
    assert_ne!(
        directory(&mut socket)["host_generation"],
        old["host_generation"]
    );
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
