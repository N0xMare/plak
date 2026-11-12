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
use std::process::Stdio;
use std::sync::mpsc;

use plakd_wire_types::PlakExtension;
use plakous_host_protocol::wire::{
    Answer, ClientFrame, Hello, HostFrame, NodeAddress, Op, Outcome, RequestId, SessionActivity,
    SessionEnvironment, SessionResidency, SessionRow, WIRE_VERSION, decode_line, encode_line,
};
use plakous_host_protocol::{Draft, SessionIntent};
use plakous_hosted::sessions::projector::RootInterruptCause;
use serde_json::json;

use super::offline_support::TestChild as Child;
use super::offline_support::*;

/// A started `plak host` and the socket its start line names.
struct Host {
    child: Option<Child>,
    socket: PathBuf,
    /// Its stderr lines after the start line.
    lines: mpsc::Receiver<String>,
}

/// Starts `plak host`; its stderr lines arrive on `lines`.
fn spawn(fixture: &Fixture) -> (Child, mpsc::Receiver<String>) {
    spawn_with(fixture, &[])
}

/// [`spawn`] with `extra` arguments to `plak host`.
fn spawn_with(fixture: &Fixture, extra: &[&str]) -> (Child, mpsc::Receiver<String>) {
    spawn_env(fixture, extra, &[])
}

/// [`spawn_with`] with `env` set for the host.
fn spawn_env(
    fixture: &Fixture,
    extra: &[&str],
    env: &[(&str, String)],
) -> (Child, mpsc::Receiver<String>) {
    let mut child = Child::spawn(
        fixture
            .command()
            .arg("host")
            .args(extra)
            .envs(env.iter().map(|(name, value)| (*name, value.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped()),
    );
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
    start_env(fixture, extra, &[])
}

fn start_env(fixture: &Fixture, extra: &[&str], env: &[(&str, String)]) -> Host {
    try_start(fixture, extra, env).expect("plak host started")
}

/// [`start_env`], or `None` when the host ended before it served.
fn try_start(fixture: &Fixture, extra: &[&str], env: &[(&str, String)]) -> Option<Host> {
    serving_host(spawn_env(fixture, extra, env))
}

fn serving_host((child, lines): (Child, mpsc::Receiver<String>)) -> Option<Host> {
    let line = loop {
        let line = lines.recv_timeout(WAIT).ok()?;
        if line.starts_with("plak host: serving on ") {
            break line;
        }
    };
    let socket = line
        .strip_prefix("plak host: serving on ")
        .and_then(|rest| rest.split(" (host ").next())
        .unwrap_or_else(|| panic!("start line: {line}"));
    assert!(line.contains(", instance "), "{line}");
    Some(Host {
        child: Some(child),
        socket: PathBuf::from(socket),
        lines,
    })
}

#[test]
fn a_host_launch_directory_containing_its_home_is_not_a_workspace_default() {
    let Some(fixture) = fixture() else { return };
    let mut child = Child::spawn(
        fixture
            .command()
            .arg("host")
            .current_dir(&fixture.root)
            .env("HOME", &fixture.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped()),
    );
    let stderr = child.stderr.take().unwrap();
    let (send, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = send.send(line);
        }
    });
    let mut host = serving_host((child, lines)).expect("host serves outside any workspace");
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Options { options }) =
        client.request(1, Op::SessionOptions { workspace: None })
    else {
        panic!("options refused")
    };
    let local = options
        .runners
        .iter()
        .find(|row| row.runner.runner_id == "local")
        .unwrap();
    assert_eq!(local.default_workspace, None);
    assert!(
        !local
            .recent_workspaces
            .iter()
            .any(|recent| recent.path == fixture.root.to_str().unwrap())
    );
    assert!(
        matches!(client.request(2, new_session(&fixture.home())), Outcome::Refused(
        plakous_host_protocol::wire::RequestRefusal::PlacementRefused { reason, .. }
    ) if matches!(*reason, plakous_host_protocol::wire::PlacementReason::WorkspaceRefused { kind: plakous_host_protocol::wire::WorkspaceRefusalKind::ProductHome, .. }))
    );
    assert_eq!(client.rows(3), []);
    stop(&mut host);
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
    retained: Option<std::collections::VecDeque<HostFrame<PlakExtension>>>,
}

impl Client {
    /// Connects and says hello; the welcome names no session.
    fn connect(host: &Host) -> Self {
        Self::connect_at(&host.socket)
    }

    /// [`Self::connect`] to the socket at `socket`.
    fn connect_at(socket: &std::path::Path) -> Self {
        let stream = UnixStream::connect(socket).unwrap();
        let mut client = Self {
            reader: BufReader::new(stream.try_clone().unwrap()),
            stream,
            retained: None,
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

    fn send(&mut self, frame: &ClientFrame<PlakExtension>) {
        self.stream
            .write_all(encode_line(frame).unwrap().as_bytes())
            .unwrap();
    }

    fn next(&mut self) -> HostFrame<PlakExtension> {
        if let Some(frame) = self
            .retained
            .as_mut()
            .and_then(std::collections::VecDeque::pop_front)
        {
            return frame;
        }
        self.read_frame()
    }

    /// Retains asynchronous observations that precede a request's response.
    fn retain_observations(&mut self) {
        self.retained = Some(std::collections::VecDeque::new());
    }

    fn read_frame(&mut self) -> HostFrame<PlakExtension> {
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
    /// before it unless observation retention was requested.
    fn request(&mut self, id: RequestId, op: Op<PlakExtension>) -> Outcome<PlakExtension> {
        self.send(&ClientFrame::Request { id, op });
        loop {
            match self.read_frame() {
                HostFrame::Response {
                    id: answered,
                    outcome,
                } if answered == id => return outcome,
                frame => {
                    if let Some(held) = &mut self.retained {
                        held.push_back(frame);
                    }
                }
            }
        }
    }

    fn rows(&mut self, id: RequestId) -> Vec<SessionRow> {
        match self.request(
            id,
            Op::ListSessions {
                workspace: None,
                runner: None,
                include_archived: false,
            },
        ) {
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

    /// Waits for `answer`, failing if this prompt is refused.
    fn prompt(&mut self, id: RequestId, attachment: u64, text: &str, answer: &str) {
        assert!(
            self.try_prompt(id, attachment, text, answer),
            "prompt {id} was refused: {text:?}"
        );
    }

    /// Returns true for `answer`, or false for a refusal of this prompt.
    fn try_prompt(&mut self, id: RequestId, attachment: u64, text: &str, answer: &str) -> bool {
        self.retained.get_or_insert_with(Default::default);
        let accepted = self.request(
            id,
            Op::Intent {
                client_id: Some(uuid::Uuid::new_v4().to_string().parse().unwrap()),
                attachment,
                intent: SessionIntent::Prompt(Draft::from(text)),
                blobs: Vec::new(),
            },
        );
        assert_eq!(accepted, Outcome::Ok(Answer::Accepted { duplicate: false }));
        let mut draft_accepted = false;
        loop {
            if let HostFrame::Event {
                attachment: observed,
                observation,
                ..
            } = self.next()
                && observed == attachment
            {
                if matches!(&observation,
                    plakous_host_protocol::SessionObservation::DraftRejected(SessionIntent::Prompt(draft))
                    if draft.text == text)
                {
                    return false;
                }
                if matches!(&observation,
                    plakous_host_protocol::SessionObservation::DraftAccepted(SessionIntent::Prompt(draft))
                    if draft.text == text)
                {
                    draft_accepted = true;
                    continue;
                }
                if draft_accepted && format!("{observation:?}").contains(answer) {
                    return true;
                }
            }
        }
    }
}

const ANSWER: &str = "the host answer";
const REQUEST: &str = "00000000-0000-4000-8000-0000000000a1";

#[test]
fn prompt_refusals_fail_strict_callers_and_return_false_for_retry_callers() {
    for (before_response, strict) in [(false, false), (true, false), (false, true), (true, true)] {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let mut client = Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            stream,
            retained: None,
        };
        let observation = plakous_host_protocol::SessionObservation::<PlakExtension>::DraftRejected(
            SessionIntent::Prompt(Draft::from("retry this prompt")),
        );
        let refusal = HostFrame::Event {
            client_id: None,
            attachment: 7,
            seq: 2,
            class: observation.class(),
            observation,
            blobs: Vec::new(),
        };
        let accepted = HostFrame::Response {
            id: 42,
            outcome: Outcome::Ok(Answer::Accepted { duplicate: false }),
        };
        let stale_answer =
            plakous_host_protocol::SessionObservation::<PlakExtension>::AssistantDelta(
                "no answer will arrive".into(),
            );
        peer.write_all(
            encode_line(&HostFrame::Event {
                client_id: None,
                attachment: 7,
                seq: 1,
                class: stale_answer.class(),
                observation: stale_answer,
                blobs: Vec::new(),
            })
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
        let frames = if before_response {
            [refusal, accepted]
        } else {
            [accepted, refusal]
        };
        for frame in frames {
            peer.write_all(encode_line(&frame).unwrap().as_bytes())
                .unwrap();
        }
        peer.shutdown(std::net::Shutdown::Write).unwrap();
        if strict {
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                client.prompt(42, 7, "retry this prompt", "no answer will arrive");
            }))
            .expect_err("a strict prompt must fail on refusal");
            assert_eq!(
                panic.downcast_ref::<String>().unwrap(),
                "prompt 42 was refused: \"retry this prompt\""
            );
        } else {
            assert!(!client.try_prompt(42, 7, "retry this prompt", "no answer will arrive"));
        }
    }
}

#[test]
fn lifecycle_real_product_archive_delete_releases_idle_actor_without_reopen() {
    let Some(fixture) = fixture() else { return };
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("session not created");
    };
    client.attach(2, &node);
    let mut admin = Client::connect(&host);
    let until = std::time::Instant::now() + WAIT;
    loop {
        let rows = admin.rows(3);
        if rows.iter().any(|row| {
            row.session == session
                && row.residency == SessionResidency::Resident
                && row.activity == SessionActivity::Idle
        }) {
            break;
        }
        assert!(
            std::time::Instant::now() < until,
            "attached actor never became idle: {rows:?}"
        );
        std::thread::yield_now();
    }
    assert_eq!(
        admin.request(
            4,
            Op::ArchiveSession {
                session: session.clone(),
                archived: true
            }
        ),
        Outcome::Ok(Answer::ArchiveUpdated {
            session: session.clone(),
            archived: true
        })
    );
    assert_eq!(admin.rows(5), []);
    assert_eq!(
        admin.request(
            6,
            Op::DeleteSession {
                session: session.clone(),
                abandon_children: false
            }
        ),
        Outcome::Ok(Answer::Deleted {
            session: session.clone()
        })
    );
    let openings = host
        .lines
        .try_iter()
        .filter(|line| line.starts_with("plak host: opening session "))
        .count();
    assert_eq!(openings, 1, "lifecycle unexpectedly reopened the runtime");
    drop(client);
    drop(admin);
    stop(&mut host);
}

fn new_session(workspace: &std::path::Path) -> Op<PlakExtension> {
    Op::NewSession {
        runner: plakous_host_protocol::wire::RunnerLabel::local(),
        profile: None,
        model: None,
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
    assert_eq!(environment, SessionEnvironment::Host);
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
        watcher.request(
            1,
            Op::WatchSessions {
                include_archived: false
            }
        ),
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

/// An embedded `plak agent` started on `session`, once it said hello.
struct Agent {
    child: Child,
    /// Its JSONL records after the hello, kept so its stdout stays read.
    _records: mpsc::Receiver<serde_json::Value>,
}

/// Starts `plak agent --session <session>`; `Ok` once it opened the session,
/// `Err` with its first JSONL error record when it was refused.
fn embedded(fixture: &Fixture, session: &str) -> Result<Agent, serde_json::Value> {
    let mut child = Child::spawn(
        fixture
            .command()
            .args(["agent", "--session", session])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()),
    );
    let stdout = child.stdout.take().unwrap();
    let (send, records) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(record) = serde_json::from_str::<serde_json::Value>(&line) {
                let _ = send.send(record);
            }
        }
    });
    loop {
        let record = records
            .recv_timeout(WAIT)
            .expect("plak agent said something");
        match record["kind"].as_str() {
            Some("hello") => {
                return Ok(Agent {
                    child,
                    _records: records,
                });
            }
            Some("error") => return Err(record),
            _ => {}
        }
    }
}

/// Waits until the directory reads `session`'s row dormant.
fn dormant(client: &mut Client, from: RequestId) -> RequestId {
    let deadline = std::time::Instant::now() + WAIT;
    let mut id = from;
    while client.rows(id)[0].residency != SessionResidency::Dormant {
        assert!(std::time::Instant::now() < deadline, "never released");
        std::thread::sleep(std::time::Duration::from_millis(200));
        id += 1;
    }
    id + 1
}

/// An embedded `plak agent` that holds a session (its runtime lease, a
/// session with no tree hub and so no custody): the directory names it with
/// its pid and its environment, the host's attach is refused
/// `tree_hosted_elsewhere` naming it and opens nothing; once the holder is
/// killed, its free process lock proves it gone, the row reads free and the
/// next attach opens the session.
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
    let attachment = client.attach(10, &node);
    client.prompt(11, attachment, "the first turn", ANSWER);
    client.request(12, Op::Detach { attachment });
    let id = dormant(&mut client, 13);
    kill_local_daemon_only(&fixture.home());
    let agent = embedded(&fixture, &session).expect("plak agent opened the session");
    let pid = agent.child.id();
    let row = client.rows(id).remove(0);
    assert_eq!(row.hosted_elsewhere.map(|held| held.pid), Some(pid));
    assert_eq!(row.environment, SessionEnvironment::Cli);
    assert_eq!(row.residency, SessionResidency::Dormant);
    match client.request(id + 1, Op::Attach { node: node.clone() }) {
        Outcome::Refused(plakous_host_protocol::wire::RequestRefusal::TreeHostedElsewhere {
            pid: named,
            ..
        }) => assert_eq!(named, pid),
        other => panic!("not refused as held elsewhere: {other:?}"),
    }
    // A second embedded face is refused naming the holder, which is not
    // plak host.
    let refused = embedded(&fixture, &session)
        .err()
        .expect("a second embedded face opened the held session");
    assert_eq!(refused["code"], "tree_hosted_elsewhere", "{refused}");
    let message = refused["message"].as_str().unwrap();
    assert!(message.contains(&format!("pid {pid}")), "{message}");
    assert!(message.contains("use that process"), "{message}");
    assert!(!message.contains("it is plak host"), "{message}");
    drop(agent);
    let row = client.rows(id + 2).remove(0);
    assert_eq!(
        row.hosted_elsewhere, None,
        "a killed holder still reads held"
    );
    assert_eq!(row.environment, SessionEnvironment::Host);
    let attachment = client.attach(id + 3, &node);
    client.prompt(id + 4, attachment, "after the holder", ANSWER);
}

/// A root turn that spawns a sub-agent, waits for it and answers: the
/// session's tree is durable, and its hub holds the tree's custody while the
/// session is open.
fn spawning_script() -> serde_json::Value {
    json!({
        "version": 1,
        "tree": true,
        "durable_tree": true,
        "repeat_last_step": true,
        "steps": [
            {"output": {"tool": {"name": "spawn_agent", "arguments": {
                "template": "review",
                "tier": "read_only",
                "description": "reviewer",
                "prompt": "review the diff",
                "output_schema_json": "{\"type\":\"object\",\"properties\":{\"summary\":{\"type\":\"string\"}},\"required\":[\"summary\"],\"additionalProperties\":false}"
            }}}, "finish": "tool_calls"},
            {"output": {"tool": {"name": "wait_agent", "arguments": {
                "agent_ids": ["$result.agent_id"]
            }}}, "finish": "tool_calls"},
            {"output": {"text": ANSWER}}
        ]
    })
}

/// The tree's stored custody holder's process id, when a record is held.
fn custody_pid(fixture: &Fixture, tree: &str) -> Option<u32> {
    use plakous_recursive_subagents::tree::TreeCustody as _;
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let store =
        plakous_recursive_subagents_store::SqliteTreeStore::open(&paths.subagents_database())
            .unwrap();
    store
        .custody(tree)
        .unwrap()
        .filter(|record| !record.released)
        .map(|record| record.holding.host.instance.pid())
}

/// A tree in custody (its root spawned a sub-agent) is held by its custody
/// holder: while another live process (this test) holds the custody of the
/// dormant tree, the directory names that process and the host's attach is
/// refused `tree_hosted_elsewhere` naming it. An embedded `plak agent` that
/// opens the session holds the tree's custody and is named; killed, its
/// stale custody reads free (its instance lock is free), and the host's next
/// attach takes the tree over, its custody now naming the host.
#[test]
fn a_tree_in_custody_is_named_by_its_holder_until_the_holder_is_gone() {
    let Some(fixture) = fixture() else { return };
    fixture.write_script(&spawning_script());
    let host = start_with(&fixture, &["--idle-release", "1"]);
    let host_pid = host.child.as_ref().unwrap().id();
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created");
    };
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, "delegate this", ANSWER);
    assert_eq!(custody_pid(&fixture, &node.tree_id), Some(host_pid));
    client.request(4, Op::Detach { attachment });
    let id = dormant(&mut client, 5);
    assert_eq!(
        custody_pid(&fixture, &node.tree_id),
        None,
        "a dormant tree is released"
    );

    // This process takes the dormant tree's custody: a live holder elsewhere.
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let store =
        plakous_recursive_subagents_store::SqliteTreeStore::open(&paths.subagents_database())
            .unwrap();
    let hold = plakous_recursive_subagents::tree::acquire_custody(&store, &node.tree_id).unwrap();
    let me = std::process::id();
    let row = client.rows(id).remove(0);
    assert_eq!(row.hosted_elsewhere.map(|held| held.pid), Some(me));
    match client.request(id + 1, Op::Attach { node: node.clone() }) {
        Outcome::Refused(plakous_host_protocol::wire::RequestRefusal::TreeHostedElsewhere {
            pid,
            ..
        }) => assert_eq!(pid, me),
        other => panic!("not refused as held elsewhere: {other:?}"),
    }
    plakous_recursive_subagents::tree::release_custody_hold(&store, hold).unwrap();
    assert_eq!(client.rows(id + 2)[0].hosted_elsewhere, None);

    // An embedded face takes it, and is killed holding it.
    kill_local_daemon_only(&fixture.home());
    let agent = embedded(&fixture, &session).expect("plak agent opened the session");
    let agent_pid = agent.child.id();
    assert_eq!(custody_pid(&fixture, &node.tree_id), Some(agent_pid));
    assert_eq!(
        client.rows(id + 3)[0]
            .hosted_elsewhere
            .as_ref()
            .map(|held| held.pid),
        Some(agent_pid)
    );
    let mut agent = agent;
    agent.child.kill().unwrap();
    agent.child.wait().unwrap();
    assert_eq!(
        custody_pid(&fixture, &node.tree_id),
        Some(agent_pid),
        "the killed holder's custody record stays held"
    );
    assert_eq!(
        client.rows(id + 4)[0].hosted_elsewhere,
        None,
        "a crashed holder's stale custody reads held"
    );
    client.attach(id + 5, &node);
    assert_eq!(
        custody_pid(&fixture, &node.tree_id),
        Some(host_pid),
        "the host did not take the tree over"
    );
}

/// One indexed commit of a traced run: its store and its ordinal there.
fn indexed_commits(trace: &std::path::Path) -> Vec<(u64, String, u64)> {
    std::fs::read_to_string(trace)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|commit| commit["indexed"] == true)
        .map(|commit| {
            (
                commit["ordinal"].as_u64().unwrap(),
                commit["store"].as_str().unwrap().to_owned(),
                commit["store_ordinal"].as_u64().unwrap(),
            )
        })
        .collect()
}

/// The last indexed commit's ordinal so far.
fn traced(trace: &std::path::Path) -> u64 {
    indexed_commits(trace).last().map_or(0, |commit| commit.0)
}

/// The cut lifecycle: a new session, its first attach (materializing it), a
/// turn, and a `release_session`. `Some` with the trace ordinals at which
/// the turn and the release began, `None` once the host was killed.
fn host_lifecycle(fixture: &Fixture, host: &Host, trace: &std::path::Path) -> Option<(u64, u64)> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut client = Client::connect(host);
        let Outcome::Ok(Answer::Created { session, node, .. }) =
            client.request(1, new_session(&fixture.workspace()))
        else {
            panic!("not created");
        };
        let attachment = client.attach(2, &node);
        let turn = traced(trace);
        client.prompt(3, attachment, "a turn before the release", ANSWER);
        let release = traced(trace);
        assert_eq!(
            client.request(4, Op::ReleaseSession { session }),
            Outcome::Ok(Answer::Released)
        );
        (turn, release)
    }))
    .ok()
}

/// `host_crash_mid_new_session`, `host_crash_mid_materialize` and
/// `host_crash_mid_release` (spec 05): the real host is killed at every
/// indexed commit of a new session's creation, its first open, a turn and
/// its release, and a restarted host recovers each cut: the replayed
/// request answers one session (never two), the crashed host reads gone,
/// an attach opens the session; no cut leaves a
/// handoff, a cut after the turn ended leaves no loss, and a cut before or
/// after the turn answers the next one.
#[test]
fn host_crash_cuts_of_create_materialize_and_release_recover() {
    let Some(probe) = fixture() else { return };
    drop(probe);
    let script = json!({
        "version": 1,
        "repeat_last_step": true,
        "steps": [{"output": {"text": ANSWER}}]
    });
    let baseline = fixture().unwrap();
    baseline.write_script(&script);
    let trace = baseline.root.join("baseline-trace.jsonl");
    let mut host = start_env(
        &baseline,
        &[],
        &[(
            plakous_offline_crash_harness::commit_trace::TRACE_ENV,
            trace.display().to_string(),
        )],
    );
    let (turn, release) = host_lifecycle(&baseline, &host, &trace).expect("the baseline ran");
    stop(&mut host);
    let commits = indexed_commits(&trace);
    assert!(commits.len() > 2, "{commits:?}");
    let mut outcomes = std::collections::BTreeMap::<&str, usize>::new();
    for (ordinal, store, store_ordinal) in &commits {
        let fixture = fixture().unwrap();
        fixture.write_script(&script);
        let cut_trace = fixture.root.join("cut-trace.jsonl");
        let env = [
            (
                plakous_offline_crash_harness::commit_trace::TRACE_ENV,
                cut_trace.display().to_string(),
            ),
            (
                plakous_offline_crash_harness::commit_trace::KILL_ENV,
                format!("{store}:{store_ordinal}"),
            ),
        ];
        if let Some(mut host) = try_start(&fixture, &[], &env) {
            if host_lifecycle(&fixture, &host, &cut_trace).is_some() {
                // The cut fell after the lifecycle (a renewal, a late write).
                *outcomes.entry("after").or_default() += 1;
                stop(&mut host);
            } else {
                let status = host.child.take().unwrap().wait().unwrap();
                assert!(
                    !status.success(),
                    "cut {store}:{store_ordinal}: the host ended cleanly"
                );
            }
        }
        eprintln!(
            "checking cut {store}:{store_ordinal} (ordinal {ordinal}; turn {turn}, release {release})"
        );
        let host = start(&fixture);
        let mut client = Client::connect(&host);
        let Outcome::Ok(Answer::Created { session, node, .. }) =
            client.request(1, new_session(&fixture.workspace()))
        else {
            panic!("cut {store}:{store_ordinal}: the replay made no session");
        };
        let rows = client.rows(2);
        assert_eq!(rows.len(), 1, "cut {store}:{store_ordinal}: {rows:?}");
        assert_eq!(
            rows[0].hosted_elsewhere, None,
            "cut {store}:{store_ordinal}: the crashed host reads alive"
        );
        let attachment = client.attach(3, &node);
        // A cut inside the turn is the open law's (a loss held, or the
        // prompt restored to a paused queue, which takes no new turn); every
        // cut before or after it answers the next turn.
        let in_turn = *ordinal >= turn && *ordinal < release;
        if !in_turn {
            client.prompt(4, attachment, "after the cut", ANSWER);
        }
        let (interrupts, decisions) = root_record(&fixture, &session);
        assert!(
            !interrupts.contains(&RootInterruptCause::Handoff),
            "cut {store}:{store_ordinal}: {interrupts:?}"
        );
        assert_eq!(
            decisions, 0,
            "cut {store}:{store_ordinal}: nothing continues by itself"
        );
        let window = if *ordinal >= release {
            assert!(
                !interrupts.contains(&RootInterruptCause::OwnerLost),
                "cut {store}:{store_ordinal} in the release lost the ended turn: {interrupts:?}"
            );
            "release"
        } else if *ordinal >= turn {
            "turn"
        } else {
            "create-or-materialize"
        };
        *outcomes.entry(window).or_default() += 1;
    }
    eprintln!("host crash cuts by window: {outcomes:?}");
    assert!(
        outcomes.contains_key("release"),
        "no cut fell in the release: {outcomes:?}"
    );
}

/// Custody precedes inspection; process-free release permits embedded open.
#[test]
fn an_embedded_face_respects_custody_then_opens_a_process_free_release() {
    let Some(fixture) = fixture() else { return };
    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"output": {"text": ANSWER}}]
    }));
    let host = start(&fixture);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created");
    };
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, "held by the host", ANSWER);
    let refused = embedded(&fixture, &session)
        .err()
        .expect("a second live session opened");
    assert_eq!(refused["code"], "tree_hosted_elsewhere", "{refused}");
    let message = refused["message"].as_str().unwrap();
    assert!(message.contains("plak host"), "{message}");
    assert_eq!(
        client.request(
            4,
            Op::ReleaseSession {
                session: session.clone()
            }
        ),
        Outcome::Ok(Answer::Released)
    );
    embedded(&fixture, &session).expect("plak agent opened the released session");
}

/// A session the host created and nothing ever opened opens in an embedded
/// face first, which creates its runtime session.
#[test]
fn a_host_created_session_nothing_opened_opens_in_an_embedded_face() {
    let Some(fixture) = fixture() else { return };
    let host = start(&fixture);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created");
    };
    embedded(&fixture, &session).expect("plak agent opened the unopened session");
}

const PREFIX: &str = "the handed-off prefix";
const CONTINUED: &str = "the continued answer";

/// The root interrupts and continuation decisions the session's product
/// record holds.
fn root_record(fixture: &Fixture, session: &str) -> (Vec<RootInterruptCause>, usize) {
    use plak::store::session_records::SessionRecordStore as _;
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let store = plak::store::session_records::SqliteSessionStore::new(paths);
    let intents = store.load_intents(session).unwrap();
    (
        intents
            .root_interrupts
            .iter()
            .map(|intent| intent.value.cause)
            .collect(),
        intents.root_continuations.len(),
    )
}

/// A host stopped while a root turn runs hands the turn off; the next
/// host's start scan continues it once by itself, with no client attached,
/// and the host after that continues nothing more.
#[test]
fn a_sigterm_hands_a_running_turn_to_the_next_host_which_continues_it_once() {
    let Some(fixture) = fixture() else { return };
    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"stream": [{"text": PREFIX}, {"stall": 30000}]}]
    }));
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created");
    };
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, "a long turn", PREFIX);
    stop(&mut host);
    let (interrupts, decisions) = root_record(&fixture, &session);
    assert_eq!(interrupts, [RootInterruptCause::Handoff]);
    assert_eq!(decisions, 0);

    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"output": {"text": CONTINUED}}]
    }));
    let mut host = start(&fixture);
    let continuing = format!("plak host: continuing session {session}");
    assert!(
        std::iter::from_fn(|| host.lines.recv_timeout(WAIT).ok()).any(|line| line == continuing),
        "the start scan did not continue the handed-off session"
    );
    let deadline = std::time::Instant::now() + WAIT;
    while root_record(&fixture, &session).1 == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "no continuation decided"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let mut client = Client::connect(&host);
    client.send(&ClientFrame::Request {
        id: 1,
        op: Op::Attach { node: node.clone() },
    });
    let deadline = std::time::Instant::now() + WAIT;
    while !format!("{:?}", client.next()).contains(CONTINUED) {
        assert!(
            std::time::Instant::now() < deadline,
            "the continuation never answered"
        );
    }
    drop(client);
    stop(&mut host);
    assert_eq!(root_record(&fixture, &session).1, 1, "one continuation");

    let host = start(&fixture);
    let mut client = Client::connect(&host);
    client.rows(1);
    assert!(
        !host
            .lines
            .try_iter()
            .any(|line| line.starts_with("plak host: continuing session ")),
        "a continued session was continued again"
    );
    assert_eq!(root_record(&fixture, &session).1, 1);
}

/// A process-free release detaches and permits an embedded open.
#[test]
fn a_released_session_is_dormant_but_keeps_its_daemon_line() {
    let Some(fixture) = fixture() else { return };
    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"output": {"text": ANSWER}}]
    }));
    let host = start(&fixture);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created");
    };
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, "before the release", ANSWER);
    client.send(&ClientFrame::Request {
        id: 4,
        op: Op::ReleaseSession {
            session: session.clone(),
        },
    });
    let mut detached = false;
    loop {
        match client.next() {
            HostFrame::Detached { attachment: ended } if ended == attachment => detached = true,
            HostFrame::Response { id: 4, outcome } => {
                assert_eq!(outcome, Outcome::Ok(Answer::Released));
                break;
            }
            _ => {}
        }
    }
    assert!(detached, "the attachment did not end with detached");
    assert_eq!(client.rows(5)[0].residency, SessionResidency::Dormant);
    embedded(&fixture, &session).expect("plak agent opened the released session");
}

/// `host_sigkill_mid_handoff` (spec 05): a host stopping with a root turn
/// running is killed during its handoff, once at once and once after the
/// handoff is durable. A session whose handoff was recorded is continued
/// once by the next host's start scan; one whose handoff was not is not
/// continued by itself, and its next open records the loss and holds it.
/// No session is continued twice.
#[test]
fn host_crash_sigkill_mid_handoff_continues_each_handed_off_turn_once() {
    for after_handoff in [false, true] {
        let Some(fixture) = fixture() else { return };
        fixture.write_script(&json!({
            "version": 1,
            "steps": [{"stream": [{"text": PREFIX}, {"stall": 30000}]}]
        }));
        let mut host = start(&fixture);
        let mut client = Client::connect(&host);
        let Outcome::Ok(Answer::Created { session, node, .. }) =
            client.request(1, new_session(&fixture.workspace()))
        else {
            panic!("not created");
        };
        let attachment = client.attach(2, &node);
        client.prompt(3, attachment, "a long turn", PREFIX);
        let pid = host.child.as_ref().unwrap().id().to_string();
        assert!(
            std::process::Command::new("kill")
                .args(["-TERM", &pid])
                .status()
                .unwrap()
                .success()
        );
        if after_handoff {
            let deadline = std::time::Instant::now() + WAIT;
            while !root_record(&fixture, &session)
                .0
                .contains(&RootInterruptCause::Handoff)
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the handoff was never recorded"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        kill(&mut host);
        drop(client);
        let handed_off = root_record(&fixture, &session)
            .0
            .contains(&RootInterruptCause::Handoff);
        assert!(!after_handoff || handed_off);
        fixture.write_script(&json!({
            "version": 1,
            "repeat_last_step": true,
            "steps": [{"output": {"text": CONTINUED}}]
        }));
        let mut host = start(&fixture);
        let continuing = format!("plak host: continuing session {session}");
        if handed_off {
            assert!(
                std::iter::from_fn(|| host.lines.recv_timeout(WAIT).ok())
                    .any(|line| line == continuing),
                "a handed-off session was not continued"
            );
            let deadline = std::time::Instant::now() + WAIT;
            while root_record(&fixture, &session).1 == 0 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "no continuation decided"
                );
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        } else {
            let mut client = Client::connect(&host);
            client.rows(1);
            assert!(
                !host.lines.try_iter().any(|line| line == continuing),
                "a turn not handed off was continued by itself"
            );
            client.attach(2, &node);
            let deadline = std::time::Instant::now() + WAIT;
            while !root_record(&fixture, &session)
                .0
                .contains(&RootInterruptCause::OwnerLost)
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the loss was never recorded"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        stop(&mut host);
        let decisions = root_record(&fixture, &session).1;
        assert_eq!(
            decisions,
            usize::from(handed_off),
            "after_handoff={after_handoff}"
        );
        // A third host continues nothing more.
        let mut host = start(&fixture);
        Client::connect(&host).rows(1);
        stop(&mut host);
        assert_eq!(root_record(&fixture, &session).1, decisions);
    }
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
                runner: plakous_host_protocol::wire::RunnerLabel::local(),
                profile: None,
                model: None,
                workspace: fixture.workspace().display().to_string(),
                template: None,
                request_id: "not-a-uuid".into(),
            }
        ),
        Outcome::Refused(plakous_host_protocol::wire::RequestRefusal::Malformed { .. })
    ));
    assert!(matches!(
        client.request(2, new_session(&fixture.root.join("nowhere"))),
        Outcome::Refused(plakous_host_protocol::wire::RequestRefusal::PlacementRefused { reason, .. })
            if matches!(reason.as_ref(), plakous_host_protocol::wire::PlacementReason::WorkspaceRefused {
                kind: plakous_host_protocol::wire::WorkspaceRefusalKind::NotFound, ..
            })
    ));
    assert!(matches!(
        client.request(
            3,
            Op::NewSession {
                runner: plakous_host_protocol::wire::RunnerLabel::local(),
                profile: None,
                model: None,
                workspace: fixture.workspace().display().to_string(),
                template: Some("no-such-template".into()),
                request_id: REQUEST.into(),
            }
        ),
        Outcome::Refused(
            plakous_host_protocol::wire::RequestRefusal::BindingRefused {
                kind: plakous_host_protocol::wire::BindingRefusalKind::UnknownTemplate,
            }
        )
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
    // The same request id with another workspace asks for something else.
    assert!(matches!(
        client.request(8, new_session(&fixture.workspace())),
        Outcome::Refused(plakous_host_protocol::wire::RequestRefusal::Malformed { .. })
    ));
    // The same request id with another template's binding asks for
    // something else too.
    write_private(
        &fixture.home().join("agents").join("other.toml"),
        "account = \"codex\"\nmodel = \"another-model\"\neffort = \"medium\"\ncomposition = \"direct\"\ndescription = \"another binding\"\n",
    );
    let request_id = "00000000-0000-4000-8000-0000000000a3";
    let made = |template: Option<&str>| Op::NewSession {
        runner: plakous_host_protocol::wire::RunnerLabel::local(),
        profile: None,
        model: None,
        workspace: fixture.workspace().display().to_string(),
        template: template.map(str::to_owned),
        request_id: request_id.into(),
    };
    assert!(matches!(
        client.request(11, made(None)),
        Outcome::Ok(Answer::Created { .. })
    ));
    assert!(matches!(
        client.request(12, made(Some("other"))),
        Outcome::Refused(plakous_host_protocol::wire::RequestRefusal::Malformed { .. })
    ));
    // A file is not a workspace.
    let file = fixture.root.join("a-file");
    std::fs::write(&file, "not a directory").unwrap();
    assert!(matches!(
        client.request(
            9,
            Op::NewSession {
                runner: plakous_host_protocol::wire::RunnerLabel::local(),
                profile: None,
                model: None,
                workspace: file.display().to_string(),
                template: None,
                request_id: "00000000-0000-4000-8000-0000000000a2".into(),
            }
        ),
        Outcome::Refused(plakous_host_protocol::wire::RequestRefusal::PlacementRefused { reason, .. })
            if matches!(reason.as_ref(), plakous_host_protocol::wire::PlacementReason::WorkspaceRefused {
                kind: plakous_host_protocol::wire::WorkspaceRefusalKind::NotDirectory, ..
            })
    ));
    assert_eq!(client.rows(10).len(), 2);
}

/// Replays of one request racing on several connections make one session,
/// and every one of them is answered it.
#[test]
fn concurrent_replays_of_one_request_make_one_session() {
    let Some(fixture) = fixture() else { return };
    let host = start(&fixture);
    let socket = host.socket.clone();
    let answers: Vec<Outcome<PlakExtension>> = std::thread::scope(|scope| {
        let replays: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    let mut client = Client::connect_at(&socket);
                    client.request(1, new_session(&fixture.workspace()))
                })
            })
            .collect();
        replays
            .into_iter()
            .map(|replay| replay.join().unwrap())
            .collect()
    });
    assert!(
        matches!(answers[0], Outcome::Ok(Answer::Created { .. })),
        "{answers:?}"
    );
    assert!(
        answers.iter().all(|answer| *answer == answers[0]),
        "{answers:?}"
    );
    assert_eq!(Client::connect(&host).rows(1).len(), 1);
}

/// A web endpoint that cannot bind stops the host before it serves, and
/// the host leaves no control socket behind.
#[test]
fn a_web_endpoint_that_cannot_bind_leaves_no_control_socket() {
    let Some(fixture) = fixture() else { return };
    let config = fixture.home().join("config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    write_private(
        &config,
        &format!(
            "{text}\n[web]\nallow = [\"a@example.com\"]\norigin = \"https://h.example.ts.net\"\n"
        ),
    );
    // The socket's directory is a file: the listener cannot be made.
    let blocked = fixture.root.join("blocked");
    std::fs::write(&blocked, "a file").unwrap();
    let (mut child, _lines) = spawn_with(
        &fixture,
        &["--web", &format!("unix:{}/web.sock", blocked.display())],
    );
    assert!(!child.wait().unwrap().success());
    let run = fixture.home().join("host/run");
    let sockets: Vec<_> = std::fs::read_dir(&run)
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default();
    assert_eq!(sockets, Vec::<PathBuf>::new(), "a control socket was left");
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

/// Unreadable custody omits only its row and never admits an open.
#[test]
fn a_broken_tree_row_degrades_the_directory_but_does_not_admit_an_open() {
    let Some(fixture) = fixture() else { return };
    let host = start(&fixture);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created");
    };
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let mut create_healthy = new_session(&fixture.workspace());
    let Op::NewSession { request_id, .. } = &mut create_healthy else {
        unreachable!();
    };
    *request_id = "00000000-0000-4000-8000-0000000000b9".into();
    let Outcome::Ok(Answer::Created { node: healthy, .. }) = client.request(2, create_healthy)
    else {
        panic!("healthy session not created");
    };
    assert_ne!(healthy, node);
    rusqlite::Connection::open(paths.subagents_database())
        .unwrap()
        .execute(
            "INSERT INTO agent_tree_custody(tree_id,fence,host_id,host_instance,host_name,claimed_at,renewed_at,released) VALUES(?1,1,'h','not an instance','m',0,0,0)",
            [&node.tree_id],
        )
        .unwrap();
    match client.request(
        3,
        Op::ListSessions {
            workspace: None,
            runner: None,
            include_archived: false,
        },
    ) {
        Outcome::Ok(Answer::Sessions { rows }) => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].node, healthy);
            assert_eq!(rows[0].profile.as_deref(), Some("custom"));
            assert_eq!(rows[0].activity_seq, 0);
        }
        other => panic!("one broken row failed the directory: {other:?}"),
    }
    let attachment = client.attach(4, &node);
    assert!(
        matches!(
            client.request(
                5,
                Op::Intent {
                    attachment,
                    intent: SessionIntent::Prompt("must not execute".into()),
                    client_id: None,
                    blobs: Vec::new(),
                }
            ),
            Outcome::Refused(_)
        ),
        "degraded directory facts must never authorize execution"
    );
    assert_eq!(crash::dispatches(&fixture), 0);
}

/// A root turn its host lost and an open held is continued by the next
/// host's start scan only under `resume_after_loss = "continue"`.
#[test]
fn a_held_loss_continues_at_the_next_start_only_under_continue() {
    let Some(fixture) = fixture() else { return };
    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"stream": [{"text": PREFIX}, {"stall": 30000}]}]
    }));
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created");
    };
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, "a turn the host loses", PREFIX);
    drop(client);
    kill(&mut host);
    fixture.write_script(&json!({
        "version": 1,
        "repeat_last_step": true,
        "steps": [{"output": {"text": CONTINUED}}]
    }));
    // The next open records the loss and holds it.
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    client.attach(1, &node);
    let deadline = std::time::Instant::now() + WAIT;
    while !root_record(&fixture, &session)
        .0
        .contains(&RootInterruptCause::OwnerLost)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the loss was never recorded"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    drop(client);
    stop(&mut host);
    let continuing = format!("plak host: continuing session {session}");
    // Under the default hold, the next host leaves it held.
    let mut host = start(&fixture);
    Client::connect(&host).rows(1);
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        !host.lines.try_iter().any(|line| line == continuing),
        "a held loss continued under hold"
    );
    stop(&mut host);
    assert_eq!(root_record(&fixture, &session).1, 0);
    // Under continue, the next host's scan continues it by itself.
    let config = fixture.home().join("config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    write_private(
        &config,
        &format!("{text}\n[host]\nresume_after_loss = \"continue\"\n"),
    );
    let mut host = start(&fixture);
    assert!(
        std::iter::from_fn(|| host.lines.recv_timeout(WAIT).ok()).any(|line| line == continuing),
        "a held loss did not continue under continue"
    );
    let deadline = std::time::Instant::now() + WAIT;
    while root_record(&fixture, &session).1 == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "no continuation decided"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    stop(&mut host);
}

/// A stop of a running turn whose attachment detaches before the stop
/// settles is still answered, with no other frame to carry it.
#[test]
fn a_stop_is_answered_after_its_attachment_detaches() {
    let Some(fixture) = fixture() else { return };
    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"stream": [{"text": PREFIX}, {"stall": 30000}]}]
    }));
    let host = start(&fixture);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created");
    };
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, "a turn to stop", PREFIX);
    client.send(&ClientFrame::Request {
        id: 4,
        op: Op::StopSession { session },
    });
    client.send(&ClientFrame::Request {
        id: 5,
        op: Op::Detach { attachment },
    });
    let mut answered = std::collections::BTreeMap::new();
    while answered.len() < 2 {
        if let HostFrame::Response { id, outcome } = client.next() {
            answered.insert(id, outcome);
        }
    }
    assert_eq!(
        answered[&4],
        Outcome::Ok(Answer::Stopped {
            runner_pending: false
        })
    );
    assert_eq!(answered[&5], Outcome::Ok(Answer::Detached));
}

mod bridge;
mod crash;
mod profiles;
mod reload;
mod requests;
mod runner;
mod singleton;
pub(crate) mod sweep;
