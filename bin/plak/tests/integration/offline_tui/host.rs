//! Socket-mode terminal laws, using the same PTY and home fixture as the
//! embedded corpus.

use super::*;
use plakous_host_protocol::wire::{ClientFrame, Hello, Op, WIRE_VERSION, encode_line};
use std::io::{BufRead as _, BufReader};
use std::os::unix::net::UnixStream;

struct Directory {
    stream: UnixStream,
    read: BufReader<UnixStream>,
    id: u64,
    binary: String,
}

impl Directory {
    fn connect(fixture: &Fixture) -> Self {
        let stream =
            UnixStream::connect(&fixture.host.lock().unwrap().as_ref().unwrap().socket).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        let mut client = Self {
            read: BufReader::new(stream.try_clone().unwrap()),
            stream,
            id: 0,
            binary: String::new(),
        };
        client.write(&ClientFrame::Hello(Hello {
            proto: WIRE_VERSION,
            binary_id: "terminal-test".into(),
            caps: Vec::new(),
        }));
        let welcome = client.next();
        assert_eq!(welcome["frame"], "welcome");
        welcome["binary_id"]
            .as_str()
            .unwrap()
            .clone_into(&mut client.binary);
        client
    }

    fn write(&mut self, frame: &ClientFrame<plakous_host_protocol::Core>) {
        self.stream
            .write_all(encode_line(frame).unwrap().as_bytes())
            .unwrap();
    }

    fn next(&mut self) -> Value {
        let mut line = String::new();
        assert!(self.read.read_line(&mut line).unwrap() > 0, "host closed");
        serde_json::from_str(&line).unwrap()
    }

    fn request(&mut self, op: Op<plakous_host_protocol::Core>) -> Value {
        self.id += 1;
        self.write(&ClientFrame::Request { id: self.id, op });
        loop {
            let frame = self.next();
            if frame["frame"] == "response" && frame["id"] == self.id {
                return frame["outcome"].clone();
            }
        }
    }

    fn rows(&mut self) -> Vec<Value> {
        self.request(Op::ListSessions { workspace: None })["ok"]["rows"]
            .as_array()
            .unwrap()
            .clone()
    }
}

pub(super) fn assert_client_reload_keeps_host(fixture: &Fixture, terminal: &mut Terminal) {
    let mut directory = Directory::connect(fixture);
    let before = directory.rows();
    let pid = fixture.host.lock().unwrap().as_ref().unwrap().child.id();
    let dispatch_counts = (provider_dispatches(fixture), child_dispatches(fixture));
    let raw_before = terminal.raw_len();
    terminal.send(b"/reload\r");
    let until = Instant::now() + WAIT;
    while !String::from_utf8_lossy(&terminal.raw_since(raw_before)).contains("reloading…") {
        assert!(Instant::now() < until, "the client never reloaded");
        std::thread::sleep(Duration::from_millis(10));
    }
    terminal.wait_ready();
    assert_eq!(
        fixture.host.lock().unwrap().as_ref().unwrap().child.id(),
        pid
    );
    assert_eq!(
        directory.rows(),
        before,
        "a client reload changes no session state"
    );
    assert_eq!(
        (provider_dispatches(fixture), child_dispatches(fixture)),
        dispatch_counts,
        "no root or child continuation is dispatched"
    );
}

pub(super) fn assert_client_loss_keeps_work(fixture: &Fixture, session: &str, answer: &str) {
    let paid = (provider_dispatches(fixture), child_dispatches(fixture));
    let mut reopened = Terminal::spawn(fixture, Launch::Resume(session));
    reopened.wait_ready();
    let screen = reopened.wait_screen(answer);
    assert!(
        !screen.contains("recovered an interrupted turn")
            && !screen.contains("a lost host interrupted")
    );
    assert_eq!(
        (provider_dispatches(fixture), child_dispatches(fixture)),
        paid,
        "killing a client never restarts its work"
    );
    reopened.quit();
}

#[test]
fn a_host_tui_refuses_requested_reload_without_touching_the_session() {
    for busy in [false, true] {
        assert_requested_reload_refused(busy);
    }
}

fn assert_requested_reload_refused(busy: bool) {
    let step = if busy {
        json!({"stream": [{"text": "host request stays here"}, {"stall": 30000}]})
    } else {
        json!({"output": {"text": "host request stays here"}})
    };
    let fixture = Fixture::with_mode(
        &json!({"version": 1, "repeat_last_step": true, "steps": [step]}),
        Mode::Host,
    );
    enable_reload_requests(&fixture);
    let installed = fixture.root.join("request-install");
    std::fs::create_dir_all(&installed).unwrap();
    install_this_build(&fixture, &installed);
    let mut terminal = Terminal::spawn(&fixture, Launch::OnPath(&installed));
    terminal.wait_ready();
    terminal.send(b"stay in this host\r");
    terminal.wait_screen("host request stays here");
    if !busy {
        terminal.wait_absent("Esc stop");
    }
    let mut directory = Directory::connect(&fixture);
    let before = directory.rows();
    let session = before[0]["session"].as_str().unwrap();
    let binary = running_binary_id(&fixture);
    let dispatches = provider_dispatches(&fixture);
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let database = rusqlite::Connection::open_with_flags(
        paths.product_database(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let intents: i64 = database
        .query_row("SELECT count(*) FROM host_intents", [], |row| row.get(0))
        .unwrap();
    let outcome = ask_reload_until_announced(&fixture, session, &binary);
    let written = wait_for_outcome(&outcome);
    assert_eq!(written["outcome"], "refused");
    let build = if directory.binary == "unknown" || directory.binary.is_empty() {
        String::new()
    } else {
        format!(" (build {})", directory.binary)
    };
    let reason = format!(
        "this session runs in plak host{build}; restarting the host moves it onto the installed build (plak host stop, or restart its unit)"
    );
    assert_eq!(written["reason"], reason);
    terminal.wait_screen("requested reload refused: this session runs in plak host");
    assert_eq!(
        provider_dispatches(&fixture),
        dispatches,
        "no continuation or redispatch"
    );
    assert_eq!(
        database
            .query_row("SELECT count(*) FROM host_intents", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        intents,
        "no host intent, continuation or handoff"
    );
    assert_eq!(
        directory.rows(),
        before,
        "the session stays resident and unchanged"
    );
    terminal.quit();
}

#[test]
fn a_host_tui_new_switches_only_its_attachment_and_quit_detaches() {
    let fixture = Fixture::with_mode(
        &json!({"version": 1, "repeat_last_step": true, "steps": [{"output": {"text": "socket turn answered"}}]}),
        Mode::Host,
    );
    let mut terminal = Terminal::spawn_ready(&fixture);
    terminal.send(b"first host turn\r");
    terminal.wait_screen("socket turn answered");
    let mut directory = Directory::connect(&fixture);
    let original = directory.rows()[0]["session"].as_str().unwrap().to_owned();
    terminal.send(b"/new\r");
    terminal.wait_absent("socket turn answered");
    terminal.send(b"second host turn\r");
    terminal.wait_screen("socket turn answered");
    let rows = directory.rows();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|row| row["session"] == original));
    terminal.quit();
    assert!(
        fixture
            .host
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .child
            .try_wait()
            .unwrap()
            .is_none()
    );
    assert!(
        directory
            .rows()
            .iter()
            .all(|row| row["residency"] == "resident")
    );
}

#[test]
fn a_host_tui_stop_stops_work_but_keeps_the_session_attached() {
    let fixture = Fixture::with_mode(
        &json!({"version": 1, "repeat_last_step": true, "steps": [{"stream": [{"text": "socket turn running"}, {"stall": 30000}]}]}),
        Mode::Host,
    );
    let mut terminal = Terminal::spawn_ready(&fixture);
    terminal.send(b"run until stopped\r");
    terminal.wait_screen("socket turn running");
    terminal.send(b"/stop\r");
    terminal.wait_where("the durable session stop", |frame| {
        frame.contains("session work stopped")
            || frame.contains("stopped: the session's work ended")
    });
    let mut directory = Directory::connect(&fixture);
    let rows = directory.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["activity"], "stopped");
    terminal.quit();
}

#[test]
fn a_host_tui_reload_replaces_only_the_client_and_keeps_the_session_resident() {
    let fixture = Fixture::with_mode(
        &json!({"version": 1, "repeat_last_step": true, "steps": [{"output": {"text": "resident before client reload"}}]}),
        Mode::Host,
    );
    let installed = fixture.root.join("installed");
    std::fs::create_dir_all(&installed).unwrap();
    install_this_build(&fixture, &installed);
    let mut terminal = Terminal::spawn(&fixture, Launch::OnPath(&installed));
    terminal.wait_ready();
    terminal.send(b"a resident session\r");
    terminal.wait_screen("resident before client reload");
    terminal.wait_absent("Esc stop");
    // A changed client binding must not carry the host's resident session.
    let mut changed = json!({"version": 1, "repeat_last_step": true, "steps": [{"output": {"text": "resident before client reload"}}]});
    changed["adapter_profile"] = json!("offline-loopback-adapter-v2");
    fixture.write_script(&changed);
    assert_client_reload_keeps_host(&fixture, &mut terminal);
    terminal.wait_screen("resident before client reload");
    terminal.quit();
}

#[test]
fn a_host_tui_refuses_a_hand_reload_that_would_select_another_state_key() {
    let fixture = Fixture::with_mode(
        &tool_then_answer("host stays on this state key"),
        Mode::Host,
    );
    let installed = fixture.root.join("different-key-install");
    std::fs::create_dir_all(&installed).unwrap();
    let mut terminal = Terminal::spawn(&fixture, Launch::OnPath(&installed));
    terminal.wait_ready();
    terminal.send(b"keep this host session\r");
    terminal.wait_screen("host stays on this state key");
    terminal.wait_absent("Esc stop");
    let (args, export) = install_carry_target(&fixture, &installed, 1000);
    terminal.send(b"/reload\r");
    terminal.wait_screen("the installed client reads another state key");
    assert!(terminal.is_alive());
    assert!(
        !args.exists() && !export.exists(),
        "no client exec or session export"
    );
    let mut directory = Directory::connect(&fixture);
    assert_eq!(directory.rows().len(), 1);
    terminal.quit();
}

#[test]
fn a_host_tui_reports_loss_and_attaches_after_the_host_restarts() {
    let fixture = Fixture::with_mode(
        &json!({"version": 1, "repeat_last_step": true, "steps": [{"output": {"text": "host transcript survives"}}]}),
        Mode::Host,
    );
    let mut terminal = Terminal::spawn_ready(&fixture);
    terminal.send(b"remember this transcript\r");
    terminal.wait_screen("host transcript survives");
    fixture.kill_host();
    terminal.wait_screen("product host disconnected");
    fixture.ensure_host();
    terminal.send(b"/attach\r");
    terminal.wait_ready();
    terminal.wait_screen("host transcript survives");
    terminal.quit();
}
