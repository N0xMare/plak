//! Startup readiness (spec 02 §4.6): Welcome and a saved first page come
//! before the start's inventories, and admission waits for them. The start
//! is held by a test hook, so every assertion is about order, not time.

use super::*;

struct Held {
    variable: &'static str,
    release: PathBuf,
}

impl Held {
    fn new(fixture: &Fixture) -> Self {
        Self {
            variable: "PLAK_TEST_HOST_STARTUP_HOLD",
            release: fixture.root.join("startup-release"),
        }
    }

    /// Holds only the session directory's first projection.
    fn directory(fixture: &Fixture) -> Self {
        Self {
            variable: "PLAK_TEST_DIRECTORY_PROJECTION_HOLD",
            release: fixture.root.join("directory-release"),
        }
    }

    fn env(&self) -> (&'static str, String) {
        (self.variable, self.release.display().to_string())
    }

    /// The inventories have not begun: the hook announced its hold and
    /// nothing has released it.
    fn holding(&self) -> bool {
        let mut held = self.release.clone().into_os_string();
        held.push(".held");
        Path::new(&held).is_file() && !self.release.exists()
    }

    fn release(&self) {
        std::fs::write(&self.release, []).unwrap();
    }
}

/// Spawns the host and returns once its socket serves, without waiting for
/// its start to complete.
fn serving_before_startup(fixture: &Fixture, held: &Held) -> Host {
    serving_held(fixture, held, &[])
}

fn serving_held(fixture: &Fixture, held: &Held, env: &[(&str, String)]) -> Host {
    let mut env = env.to_vec();
    env.push(held.env());
    let (child, lines) = spawn_env(fixture, &[], &env);
    let socket = loop {
        let line = lines.recv_timeout(WAIT).expect("the held host serves");
        if let Some(rest) = line.strip_prefix("plakd host: serving on ") {
            break PathBuf::from(rest.split(" (host ").next().unwrap());
        }
    };
    let deadline = Instant::now() + WAIT;
    while !held.holding() {
        assert!(
            Instant::now() < deadline,
            "the start never reached its hold"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    Host {
        child: Some(child),
        socket,
        lines,
    }
}

fn hello(socket: &Path) -> (BufReader<UnixStream>, UnixStream) {
    let stream = UnixStream::connect(socket).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    let mut writer = stream.try_clone().unwrap();
    writer
        .write_all(
            encode_line(&ClientFrame::<PlakExtension>::Hello(Hello {
                proto: WIRE_VERSION,
                binary_id: "startup-test".into(),
                caps: Vec::new(),
            }))
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
    let mut reader = BufReader::new(stream);
    assert_eq!(frame(&mut reader)["frame"], "welcome");
    (reader, writer)
}

fn frame(reader: &mut BufReader<UnixStream>) -> serde_json::Value {
    let mut line = String::new();
    assert_ne!(reader.read_line(&mut line).unwrap(), 0, "the host closed");
    serde_json::from_str(&line).unwrap()
}

fn send(writer: &mut UnixStream, id: u64, op: &serde_json::Value) {
    let line = json!({"frame":"request","id":id,"op":op}).to_string() + "\n";
    writer.write_all(line.as_bytes()).unwrap();
}

/// The frames up to and including the response to `id`.
fn until_response(reader: &mut BufReader<UnixStream>, id: u64) -> Vec<serde_json::Value> {
    let mut frames = Vec::new();
    loop {
        let next = frame(reader);
        let done = next["frame"] == "response" && next["id"] == id;
        frames.push(next);
        if done {
            return frames;
        }
    }
}

fn lifecycle(frame: &serde_json::Value, phase: &str) -> bool {
    frame["frame"] == "host_lifecycle" && frame["phase"] == phase && frame["reason"] == "startup"
}

fn reconciling(outcome: &serde_json::Value) -> bool {
    let refused = outcome["refused"].to_string();
    refused.contains("placement_refused")
        && refused.contains("runner_unreachable")
        && refused.contains("reconcile")
}

#[test]
fn a_held_start_serves_welcome_and_a_saved_first_page_and_admission_waits() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let session = seed(&fixture, 41);
    let mut host = start(&fixture);
    let node = Client::connect(&host)
        .rows(1)
        .into_iter()
        .find(|row| row.session == session)
        .unwrap()
        .node;
    stop(&mut host);

    let held = Held::new(&fixture);
    let mut host = serving_before_startup(&fixture, &held);
    let (mut reader, mut writer) = hello(&host.socket);
    assert!(held.holding(), "Welcome came after the inventories began");
    assert!(lifecycle(&frame(&mut reader), "starting"));

    send(
        &mut writer,
        1,
        &json!({"op":"attach","node":{"tree_id":node.tree_id,"agent_id":node.agent_id}}),
    );
    let attached = until_response(&mut reader, 1);
    let answer = &attached.last().unwrap()["outcome"]["ok"];
    assert_eq!(answer["answer"], "attached", "{attached:?}");
    assert_eq!(answer["transcript"]["mode"], "first_page", "{answer}");
    let attachment = answer["attachment"].clone();
    let mut page = Vec::new();
    while !page
        .last()
        .is_some_and(|part: &serde_json::Value| part["last"] == true)
    {
        let next = frame(&mut reader);
        if next["frame"] == "snapshot" && next["attachment"] == attachment {
            page.push(next);
        }
    }
    assert!(held.holding(), "the saved page waited for the inventories");

    send(
        &mut writer,
        2,
        &json!({"op":"intent","attachment":attachment,"client_id":uuid::Uuid::new_v4().to_string(),
            "intent":{"kind":"prompt","body":{"text":"not before the start","images":[]}},"blobs":[]}),
    );
    let refused = until_response(&mut reader, 2);
    assert!(
        reconciling(&refused.last().unwrap()["outcome"]),
        "{refused:?}"
    );

    // The picker needs the runner inventory: it waits rather than answer.
    let (mut picker, mut picker_writer) = hello(&host.socket);
    assert!(lifecycle(&frame(&mut picker), "starting"));
    send(&mut picker_writer, 1, &json!({"op":"session_options"}));
    picker
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    let mut line = String::new();
    assert!(
        picker.read_line(&mut line).is_err(),
        "the picker answered before the start: {line}"
    );
    assert!(held.holding());
    picker.get_ref().set_read_timeout(Some(WAIT)).unwrap();

    held.release();
    let answered = until_response(&mut picker, 1);
    assert!(
        answered.last().unwrap()["outcome"]["ok"]["answer"] == "options",
        "{answered:?}"
    );
    let mut running = frame(&mut reader);
    while !lifecycle(&running, "running") {
        running = frame(&mut reader);
    }
    // The host opens the session the link waited for and moves it live; the
    // client sends no retry.
    let moved = loop {
        let next = frame(&mut reader);
        if next["frame"] == "moved" {
            break next;
        }
    };
    assert_eq!(moved["attachment"], attachment, "{moved}");
    loop {
        let line = host.lines.recv_timeout(WAIT).unwrap();
        if line.starts_with("plakd host: startup complete") {
            break;
        }
        assert!(!line.starts_with("plakd host: startup failed"), "{line}");
    }

    // After the start the same node also attaches live.
    let mut client = Client::connect(&host);
    client.attach(3, &node);
    drop(client);
    stop(&mut host);
}

fn request(writer: &mut UnixStream, id: u64, op: Op<PlakExtension>) {
    writer
        .write_all(
            encode_line(&ClientFrame::<PlakExtension>::Request { id, op })
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
}

/// Nothing arrives on `reader` while `held` holds: a request it carries is
/// still waiting.
fn silent(reader: &mut BufReader<UnixStream>, held: &Held) {
    reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    let mut line = String::new();
    assert!(
        reader.read_line(&mut line).is_err(),
        "answered while held: {line}"
    );
    assert!(held.holding());
    reader.get_ref().set_read_timeout(Some(WAIT)).unwrap();
}

#[test]
fn lifecycle_requests_wait_for_a_held_start_then_are_admitted() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let archived = seed(&fixture, 51);
    let deleted = seed(&fixture, 52);
    let stopped = seed(&fixture, 53);
    let held = Held::new(&fixture);
    let mut host = serving_before_startup(&fixture, &held);
    let requests = [
        new_session(&fixture.workspace()),
        Op::ArchiveSession {
            session: archived,
            archived: true,
        },
        Op::DeleteSession {
            session: deleted,
            abandon_children: false,
            abandon_jobs: false,
        },
        Op::StopSession { session: stopped },
    ];
    let mut waiting = Vec::new();
    for op in requests {
        let (mut reader, mut writer) = hello(&host.socket);
        assert!(lifecycle(&frame(&mut reader), "starting"));
        let name = op.name();
        request(&mut writer, 1, op);
        waiting.push((name, reader, writer));
    }
    for (_, reader, _) in &mut waiting {
        silent(reader, &held);
    }
    held.release();
    for (name, reader, _) in &mut waiting {
        let answered = until_response(reader, 1);
        let outcome = &answered.last().unwrap()["outcome"];
        // Admitted: answered by its own checks, not the startup refusal.
        assert!(!reconciling(outcome), "{name} after the start: {outcome}");
        if *name == "new_session" {
            assert_eq!(outcome["ok"]["answer"], "created", "{outcome}");
        }
    }
    stop(&mut host);
}

#[test]
fn a_stop_during_a_held_start_sends_no_running_and_joins_the_start() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let held = Held::new(&fixture);
    let mut host = serving_before_startup(&fixture, &held);
    let (mut reader, _writer) = hello(&host.socket);
    assert!(lifecycle(&frame(&mut reader), "starting"));
    stop(&mut host);
    let mut phases = Vec::new();
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap_or(0) != 0 {
        let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
        if frame["frame"] == "host_lifecycle" {
            phases.push(frame["phase"].as_str().unwrap().to_owned());
        }
        line.clear();
    }
    assert_eq!(phases, ["stopping"], "a stopped start announced running");
    assert!(!held.release.exists());
    let lines: Vec<String> = host.lines.iter().collect();
    let ended = lines
        .iter()
        .position(|line| line == "plakd host: startup ended by the stop")
        .unwrap_or_else(|| panic!("the start was not joined: {lines:?}"));
    let stopped = lines
        .iter()
        .position(|line| line.starts_with("plakd host: stopped in "))
        .unwrap();
    assert!(ended < stopped, "{lines:?}");
    assert!(
        !lines
            .iter()
            .any(|line| line.starts_with("plakd host: startup complete")),
        "{lines:?}"
    );
}

#[test]
fn a_failed_startup_inventory_stops_the_host_with_a_failed_exit() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let (mut child, lines) = spawn_env(
        &fixture,
        &[],
        &[(
            "PLAK_TEST_HOST_STARTUP_FAILURE",
            "fixture inventory unreadable".to_owned(),
        )],
    );
    let lines: Vec<String> = lines.iter().collect();
    let status = child.wait().unwrap();
    assert!(!status.success(), "{lines:?}");
    let serving = lines
        .iter()
        .position(|line| line.starts_with("plakd host: serving on "))
        .unwrap_or_else(|| panic!("{lines:?}"));
    let failed = lines
        .iter()
        .position(|line| line == "plakd host: startup failed: fixture inventory unreadable")
        .unwrap_or_else(|| panic!("{lines:?}"));
    assert!(serving < failed, "{lines:?}");
}

#[test]
fn directory_reads_wait_for_the_first_projection_and_watches_snapshot_it() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let session = seed(&fixture, 61);
    let held = Held::directory(&fixture);
    let mut host = serving_held(&fixture, &held, &[]);
    // The start completes without its directory; no notice interleaves.
    await_startup(&host.lines).unwrap();
    let (mut lister, mut list_writer) = hello(&host.socket);
    request(
        &mut list_writer,
        1,
        Op::ListSessions {
            workspace: None,
            runner: None,
            include_archived: false,
        },
    );
    let (mut watcher, mut watch_writer) = hello(&host.socket);
    request(
        &mut watch_writer,
        1,
        Op::WatchSessions {
            include_archived: false,
        },
    );
    let watching = until_response(&mut watcher, 1);
    assert_eq!(
        watching.last().unwrap()["outcome"]["ok"]["answer"],
        "watching",
        "{watching:?}"
    );
    assert!(
        !watching.iter().any(|frame| frame["frame"] == "directory"),
        "{watching:?}"
    );
    silent(&mut lister, &held);
    silent(&mut watcher, &held);
    held.release();
    let answer = until_response(&mut lister, 1);
    let rows = answer.last().unwrap()["outcome"]["ok"]["rows"].clone();
    assert!(rows.to_string().contains(&session), "{answer:?}");
    let snapshot = loop {
        let next = frame(&mut watcher);
        if next["frame"] == "directory" {
            break next;
        }
    };
    assert_eq!(snapshot["snapshot"], true, "{snapshot}");
    assert!(
        snapshot["rows"].to_string().contains(&session),
        "{snapshot}"
    );
    stop(&mut host);
}
