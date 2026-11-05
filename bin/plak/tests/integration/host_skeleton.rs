//! `plak host` end to end, as the skeleton stands: the real binary takes
//! its home's host lock, serves the control socket to its own user only,
//! refuses every hello `unavailable`, refuses a second host by name, and
//! stops on `SIGTERM`, removing its socket.
//!
//! Every wait is for an event (a line on stderr, a frame, an exit) bounded
//! by the fixture's liveness deadline.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    missing_docs,
    reason = "private real-process test harness"
)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::mpsc;

use plakous_host_protocol::Core;
use plakous_host_protocol::wire::{HandshakeRefusal, HostFrame, decode_line};
use serde_json::json;

use super::offline_support::*;

/// A started `plak host` and the socket its start line names.
struct Host {
    child: Option<Child>,
    socket: PathBuf,
}

impl Drop for Host {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Starts `plak host`; its stderr lines arrive on `lines`.
fn spawn(fixture: &Fixture) -> (Child, mpsc::Receiver<String>) {
    let mut child = fixture
        .command()
        .arg("host")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("plak host spawned");
    let stderr = child.stderr.take().unwrap();
    let (send, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = send.send(line);
        }
    });
    (child, lines)
}

fn start(fixture: &Fixture) -> Host {
    let (child, lines) = spawn(fixture);
    let line = loop {
        let line = lines.recv_timeout(WAIT).expect("plak host started");
        if line.starts_with("plak host: serving on ") {
            break line;
        }
    };
    let socket = line
        .strip_prefix("plak host: serving on ")
        .and_then(|rest| rest.split(" (host ").next())
        .unwrap_or_else(|| panic!("start line: {line}"));
    assert!(line.contains(", instance "), "{line}");
    Host {
        child: Some(child),
        socket: PathBuf::from(socket),
    }
}

/// The host fixture, or `None` where this platform's socket path limit is
/// shorter than the fixture's home allows.
fn fixture() -> Option<Fixture> {
    let fixture = Fixture::new(&json!({"version": 1, "steps": []}));
    let socket = fixture.home().join("host/run/r00.e00.p00.t00.sock");
    (socket.as_os_str().len() < 100).then_some(fixture)
}

/// A connection of the host's own user is refused `unavailable` after its
/// hello, in a wire v2 frame; the host keeps serving the next one.
#[test]
fn a_hello_on_the_host_socket_is_refused_unavailable() {
    let Some(fixture) = fixture() else { return };
    let host = start(&fixture);
    assert_eq!(
        std::fs::metadata(&host.socket)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(host.socket.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    for _ in 0..2 {
        let mut stream = UnixStream::connect(&host.socket).unwrap();
        stream.write_all(b"{\"hello\":{}}\n").unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        let frame: HostFrame<Core> = decode_line(line.trim_end()).unwrap();
        assert!(
            matches!(
                &frame,
                HostFrame::Refused(HandshakeRefusal::Unavailable { detail })
                    if detail == "plak host serves no session yet"
            ),
            "{line}"
        );
    }
}

/// A second host of the same home and state key refuses, naming the first
/// one's pid; a `SIGTERM` stops the first, which removes its socket and
/// releases the lock, so the next host starts.
#[test]
fn a_second_host_refuses_and_a_sigterm_stops_the_first() {
    let Some(fixture) = fixture() else { return };
    let mut first = start(&fixture);
    let (mut second, lines) = spawn(&fixture);
    let status = second.wait().unwrap();
    assert!(!status.success());
    let refusal: Vec<String> = lines.iter().collect();
    let pid = first.child.as_ref().unwrap().id().to_string();
    assert!(
        refusal
            .iter()
            .any(|line| line.contains("plak host is already running") && line.contains(&pid)),
        "{refusal:?}"
    );
    assert!(first.socket.exists());

    let killed = std::process::Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap();
    assert!(killed.success());
    let (done, exited) = mpsc::channel();
    let mut child = first.child.take().unwrap();
    std::thread::spawn(move || {
        let _ = done.send(child.wait());
    });
    let status = exited
        .recv_timeout(WAIT)
        .expect("plak host stopped")
        .unwrap();
    assert!(status.success(), "{status}");
    assert!(!first.socket.exists());
    let next = start(&fixture);
    assert_eq!(next.socket, first.socket);
}
