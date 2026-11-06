//! `plak host` end to end: the real binary takes its home's host lock,
//! serves its sessions' directory on the control socket to its own user
//! only, creates, opens and serves sessions there, refuses a second host by
//! name, and stops on `SIGTERM`, handing its sessions off and removing its
//! socket.
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
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::mpsc;

use plakous_host_protocol::wire::{
    Answer, ClientFrame, Hello, HostFrame, NodeAddress, Op, Outcome, RequestId, SessionActivity,
    SessionEnvironment, SessionResidency, SessionRow, WIRE_VERSION, decode_line, encode_line,
};
use plakous_host_protocol::{Core, Draft, SessionIntent};
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
    spawn_with(fixture, &[])
}

/// [`spawn`] with `extra` arguments to `plak host`.
fn spawn_with(fixture: &Fixture, extra: &[&str]) -> (Child, mpsc::Receiver<String>) {
    let mut child = fixture
        .command()
        .arg("host")
        .args(extra)
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
    start_with(fixture, &[])
}

fn start_with(fixture: &Fixture, extra: &[&str]) -> Host {
    let (child, lines) = spawn_with(fixture, extra);
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

/// One connection to the host's socket.
struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    /// Connects and says hello; the welcome names no session.
    fn connect(host: &Host) -> Self {
        let stream = UnixStream::connect(&host.socket).unwrap();
        let mut client = Self {
            reader: BufReader::new(stream.try_clone().unwrap()),
            stream,
        };
        client.send(&ClientFrame::Hello(Hello {
            proto: WIRE_VERSION,
            binary_id: "host-test".into(),
            caps: Vec::new(),
        }));
        let HostFrame::Welcome(welcome) = client.next() else {
            panic!("not welcomed");
        };
        assert_eq!(welcome.session, None, "a host of many sessions names none");
        client
    }

    fn send(&mut self, frame: &ClientFrame<Core>) {
        self.stream
            .write_all(encode_line(frame).unwrap().as_bytes())
            .unwrap();
    }

    fn next(&mut self) -> HostFrame<Core> {
        let mut line = String::new();
        self.stream.set_read_timeout(Some(WAIT)).unwrap();
        assert_ne!(
            self.reader.read_line(&mut line).unwrap(),
            0,
            "the host closed"
        );
        decode_line(line.trim_end()).unwrap_or_else(|error| panic!("{error}: {line}"))
    }

    /// Sends request `id` and returns its outcome, skipping what comes
    /// before it.
    fn request(&mut self, id: RequestId, op: Op<Core>) -> Outcome {
        self.send(&ClientFrame::Request { id, op });
        loop {
            if let HostFrame::Response {
                id: answered,
                outcome,
            } = self.next()
                && answered == id
            {
                return outcome;
            }
        }
    }

    fn rows(&mut self, id: RequestId) -> Vec<SessionRow> {
        match self.request(id, Op::ListSessions { workspace: None }) {
            Outcome::Ok(Answer::Sessions { rows }) => rows,
            other => panic!("not listed: {other:?}"),
        }
    }

    fn attach(&mut self, id: RequestId, node: &NodeAddress) -> u64 {
        match self.request(id, Op::Attach { node: node.clone() }) {
            Outcome::Ok(Answer::Attached { attachment }) => attachment,
            other => panic!("not attached: {other:?}"),
        }
    }

    /// Prompts on `attachment` and waits for `answer` in its events.
    fn prompt(&mut self, id: RequestId, attachment: u64, text: &str, answer: &str) {
        let accepted = self.request(
            id,
            Op::Intent {
                attachment,
                intent: SessionIntent::Prompt(Draft::from(text)),
                blobs: Vec::new(),
            },
        );
        assert_eq!(accepted, Outcome::Ok(Answer::Accepted));
        loop {
            if let HostFrame::Event { observation, .. } = self.next()
                && format!("{observation:?}").contains(answer)
            {
                return;
            }
        }
    }
}

const ANSWER: &str = "the host answer";
const REQUEST: &str = "00000000-0000-4000-8000-0000000000a1";

fn new_session(workspace: &std::path::Path) -> Op<Core> {
    Op::NewSession {
        workspace: workspace.display().to_string(),
        template: None,
        request_id: REQUEST.into(),
    }
}

fn stop(host: &mut Host) {
    let pid = host.child.as_ref().unwrap().id().to_string();
    let killed = std::process::Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap();
    assert!(killed.success());
    let (done, exited) = mpsc::channel();
    let mut child = host.child.take().unwrap();
    std::thread::spawn(move || {
        let _ = done.send(child.wait());
    });
    let status = exited
        .recv_timeout(WAIT)
        .expect("plak host stopped")
        .unwrap();
    assert!(status.success(), "{status}");
}

/// A client creates a session in a workspace, and a retry of the same
/// request answers the same session; the directory lists it dormant with
/// the host's environment; an attach opens it, a prompt reads the scripted
/// answer, and a watch shows it resident. A stop hands it off, and the next
/// host lists it dormant and serves it again.
#[test]
fn a_client_creates_attaches_and_prompts_a_session_across_host_restarts() {
    let Some(fixture) = fixture() else { return };
    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"output": {"text": ANSWER}}, {"output": {"text": ANSWER}}]
    }));
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    let created = client.request(1, new_session(&fixture.workspace()));
    let Outcome::Ok(Answer::Created {
        session,
        node,
        environment,
    }) = created.clone()
    else {
        panic!("not created: {created:?}");
    };
    assert_eq!(environment, SessionEnvironment::HostLoginShell);
    assert_eq!(
        client.request(2, new_session(&fixture.workspace())),
        created
    );
    let rows = client.rows(3);
    assert_eq!(rows.len(), 1, "one session for one request: {rows:?}");
    assert_eq!(rows[0].session, session);
    assert_eq!(rows[0].node, node);
    assert_eq!(rows[0].residency, SessionResidency::Dormant);
    assert_eq!(rows[0].hosted_elsewhere, None);
    let attachment = client.attach(4, &node);
    client.prompt(5, attachment, "hello from the host test", ANSWER);
    let mut watcher = Client::connect(&host);
    assert!(matches!(
        watcher.request(1, Op::WatchSessions),
        Outcome::Ok(Answer::Watching { .. })
    ));
    let HostFrame::Directory {
        seq,
        snapshot,
        rows,
        ..
    } = watcher.next()
    else {
        panic!("no directory");
    };
    assert_eq!((seq, snapshot), (1, true));
    assert_eq!(rows[0].residency, SessionResidency::Resident);
    assert_ne!(rows[0].activity, SessionActivity::HeldAfterLoss);
    drop(client);
    drop(watcher);
    stop(&mut host);
    assert!(!host.socket.exists());

    let host = start(&fixture);
    let mut client = Client::connect(&host);
    let rows = client.rows(1);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].residency, SessionResidency::Dormant);
    assert_eq!(
        rows[0].hosted_elsewhere, None,
        "a stopped host holds nothing"
    );
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, "after the restart", ANSWER);
}

fn kill(host: &mut Host) {
    let mut child = host.child.take().unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
}

/// A host killed after it answered `created` left the session durable: the
/// next host answers the same request with the same session and lists it
/// once. A host killed while it held a session resident left its custody
/// naming a dead instance: the next host lists the session dormant and
/// free, never held elsewhere, and its attach takes the tree over.
#[test]
fn a_crashed_hosts_sessions_read_free_and_open_again() {
    let Some(fixture) = fixture() else { return };
    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"output": {"text": ANSWER}}, {"output": {"text": ANSWER}}]
    }));
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    let created = client.request(1, new_session(&fixture.workspace()));
    drop(client);
    kill(&mut host);
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    assert_eq!(
        client.request(1, new_session(&fixture.workspace())),
        created
    );
    let rows = client.rows(2);
    assert_eq!(rows.len(), 1, "{rows:?}");
    let node = rows[0].node.clone();
    let attachment = client.attach(3, &node);
    client.prompt(4, attachment, "before the crash", ANSWER);
    drop(client);
    kill(&mut host);
    // The dead host's socket is replaced by the next one under its lock.
    let host = start(&fixture);
    let mut client = Client::connect(&host);
    let rows = client.rows(1);
    assert_eq!(rows[0].residency, SessionResidency::Dormant);
    assert_eq!(
        rows[0].hosted_elsewhere, None,
        "a crashed holder is proven gone by its free instance lock"
    );
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, "after the crash", ANSWER);
}

/// An embedded `plak agent` that holds a session: the host's attach opens
/// no second live session (its open is refused while the agent holds the
/// session's lease); once the holder is killed, its free process lock
/// proves it gone and the next attach opens the session.
#[test]
fn a_session_an_embedded_face_holds_is_refused_until_its_holder_is_gone() {
    let Some(fixture) = fixture() else { return };
    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"output": {"text": ANSWER}}, {"output": {"text": ANSWER}}]
    }));
    let host = start_with(&fixture, &["--idle-release", "1"]);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created");
    };
    // The host's first open creates the runtime session; once it released
    // the idle session, an embedded face may open it.
    let attachment = client.attach(10, &node);
    client.prompt(11, attachment, "the first turn", ANSWER);
    client.request(12, Op::Detach { attachment });
    let deadline = std::time::Instant::now() + WAIT;
    let mut id = 13;
    while client.rows(id)[0].residency != SessionResidency::Dormant {
        assert!(std::time::Instant::now() < deadline, "never released");
        std::thread::sleep(std::time::Duration::from_millis(200));
        id += 1;
    }
    let mut agent = fixture
        .command()
        .args(["agent", "--session", &session])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = agent.stdout.take().unwrap();
    let (said, hello) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.contains("\"kind\":\"hello\"") {
                let _ = said.send(());
            }
        }
    });
    hello
        .recv_timeout(WAIT)
        .expect("plak agent opened the session");
    match client.request(3, Op::Attach { node: node.clone() }) {
        Outcome::Refused(plakous_host_protocol::wire::RequestRefusal::SessionUnavailable {
            detail,
        }) => assert!(detail.contains("lease is held"), "{detail}"),
        other => panic!("a second live session opened: {other:?}"),
    }
    agent.kill().unwrap();
    agent.wait().unwrap();
    let attachment = client.attach(5, &node);
    client.prompt(6, attachment, "after the holder", ANSWER);
}

/// A request id that is not a uuid, or a workspace that is not a
/// directory, creates nothing.
#[test]
fn a_new_session_that_cannot_be_made_creates_nothing() {
    let Some(fixture) = fixture() else { return };
    let host = start(&fixture);
    let mut client = Client::connect(&host);
    assert!(matches!(
        client.request(
            1,
            Op::NewSession {
                workspace: fixture.workspace().display().to_string(),
                template: None,
                request_id: "not-a-uuid".into(),
            }
        ),
        Outcome::Refused(plakous_host_protocol::wire::RequestRefusal::Malformed { .. })
    ));
    assert!(matches!(
        client.request(2, new_session(&fixture.root.join("nowhere"))),
        Outcome::Refused(plakous_host_protocol::wire::RequestRefusal::SessionUnavailable { .. })
    ));
    assert!(matches!(
        client.request(
            3,
            Op::NewSession {
                workspace: fixture.workspace().display().to_string(),
                template: Some("no-such-template".into()),
                request_id: REQUEST.into(),
            }
        ),
        Outcome::Refused(plakous_host_protocol::wire::RequestRefusal::SessionUnavailable { .. })
    ));
    assert_eq!(client.rows(4), Vec::new());
    // A replay answers the session its request made, whatever became of
    // the workspace since.
    let made = fixture.workspace().join("made");
    std::fs::create_dir_all(&made).unwrap();
    let created = client.request(5, new_session(&made));
    assert!(
        matches!(created, Outcome::Ok(Answer::Created { .. })),
        "{created:?}"
    );
    std::fs::remove_dir(&made).unwrap();
    assert_eq!(client.request(6, new_session(&made)), created);
    assert_eq!(client.rows(7).len(), 1);
}

/// An idle session releases itself, the directory reads it dormant, and the
/// next attach opens it again.
#[test]
fn an_idle_session_releases_and_the_next_attach_opens_it() {
    let Some(fixture) = fixture() else { return };
    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"output": {"text": ANSWER}}, {"output": {"text": ANSWER}}]
    }));
    let host = start_with(&fixture, &["--idle-release", "1"]);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created");
    };
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, "before the release", ANSWER);
    assert_eq!(
        client.request(4, Op::Detach { attachment }),
        Outcome::Ok(Answer::Detached)
    );
    let deadline = std::time::Instant::now() + WAIT;
    let mut id = 5;
    loop {
        id += 1;
        if client.rows(id)[0].residency == SessionResidency::Dormant {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "never released");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let attachment = client.attach(id + 1, &node);
    client.prompt(id + 2, attachment, "after the release", ANSWER);
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
