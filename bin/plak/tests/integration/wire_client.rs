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
    assert_ne!(welcome.session_id, "");
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
