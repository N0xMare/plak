//! The wire v2 client ([`plakous_host_protocol::client`]) against the real
//! `plak agent --wire v2` over its stdio, on the scripted offline model.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    missing_docs,
    reason = "private real-process test harness"
)]

use std::process::Stdio;
use std::time::{Duration, Instant};

use plak::wire_transport::PlakExtension;
use plakous_host_protocol::client;
use plakous_host_protocol::wire::{Hello, WIRE_VERSION};
use plakous_host_protocol::{Draft, SessionIntent, SessionObservation};
use serde_json::json;

use super::offline_support::*;

const ANSWER: &str = "the wire client answer";

#[test]
fn a_real_wire_runtime_settings_save_preserves_nondefault_terminal_preferences() {
    use std::io::Write as _;
    let fixture = Fixture::new(&json!({"version":1,"steps":[]}));
    let config_path = fixture.home().join("config.toml");
    std::fs::OpenOptions::new().append(true).open(&config_path).unwrap()
        .write_all(b"\n[tui]\ntheme = \"catppuccin-latte\"\nmouse = false\nscroll_lines = 9\nshow_thinking = false\ngroup_tool_calls = false\nreduced_motion = true\n").unwrap();
    let before: toml::Value =
        toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    let mut child = fixture
        .command()
        .args(["agent", "--wire", "v2"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let transport =
        client::Transport::new(child.stdout.take().unwrap(), child.stdin.take().unwrap());
    let (sender, receiver, _) = client::connect::<PlakExtension>(
        transport,
        Hello {
            proto: WIRE_VERSION,
            binary_id: "settings-wire-test".into(),
            caps: Vec::new(),
        },
    )
    .unwrap();
    sender
        .send(SessionIntent::Extension(
            serde_json::from_value(json!("open_settings")).unwrap(),
        ))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut settings = loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .expect("settings timed out");
        if let SessionObservation::Extension(extension) = receiver.recv_timeout(left).unwrap() {
            let extension = serde_json::to_value(extension).unwrap();
            if let Some(view) = extension.get("settings_view") {
                break view.clone();
            }
        }
    };
    settings["product"]["compaction"] = json!(false);
    let request = SessionIntent::<PlakExtension>::Extension(
        serde_json::from_value(json!({"save_settings": settings})).unwrap(),
    );
    assert!(!serde_json::to_string(&request).unwrap().contains("\"tui\""));
    sender.send(request).unwrap();
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .expect("save timed out");
        if let SessionObservation::Extension(extension) = receiver.recv_timeout(left).unwrap() {
            let extension = serde_json::to_value(extension).unwrap();
            if let Some(view) = extension.get("settings_view_applied") {
                assert_eq!(view["product"]["compaction"], json!(false));
                break;
            }
            if let Some(reason) = extension.get("settings_save_failed") {
                panic!("save refused: {reason}");
            }
        }
    }
    let after: toml::Value =
        toml::from_str(&std::fs::read_to_string(config_path).unwrap()).unwrap();
    assert_eq!(after["tui"], before["tui"]);
    assert_eq!(after["compaction"]["enabled"], toml::Value::Boolean(false));
    drop((sender, receiver));
    let until = Instant::now() + Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < until,
            "agent did not leave after its client"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(child.wait().unwrap().success());
}

/// A client connects through the welcome, sends a prompt the session runs
/// on the offline model, receives the answer's stream and the turn's end,
/// and leaves with a bye, which ends `plak agent`.
#[test]
fn the_wire_client_drives_plak_agent_over_stdio() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "repeat_last_step": true,
        "steps": [{"output": {"text": ANSWER}}]
    }));
    let mut child = fixture
        .command()
        .args(["agent", "--wire", "v2"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("plak agent spawned");
    let transport = client::Transport::new(
        child.stdout.take().expect("stdout piped"),
        child.stdin.take().expect("stdin piped"),
    );
    let hello = Hello {
        proto: WIRE_VERSION,
        binary_id: "wire-client-test".into(),
        caps: Vec::new(),
    };
    let (sender, receiver, welcome) =
        client::connect::<PlakExtension>(transport, hello).expect("welcomed");
    assert_ne!(welcome.session.expect("a served session").session_id, "");
    sender
        .send(SessionIntent::Prompt(Draft::from("answer, please")))
        .unwrap_or_else(|refused| panic!("{refused:?}: {:?}", receiver.ended()));
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut streamed = String::new();
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("no answer in time; streamed {streamed:?}"));
        match receiver.recv_timeout(left).expect("an observation") {
            SessionObservation::AssistantDelta(delta) => streamed.push_str(&delta),
            SessionObservation::AssistantDone if streamed.contains(ANSWER) => break,
            _ => {}
        }
    }
    drop((sender, receiver));
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "plak agent left after the bye");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "{status:?}");
}
