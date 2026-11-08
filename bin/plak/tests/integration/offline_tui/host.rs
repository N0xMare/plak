//! Socket-mode terminal laws, using the same PTY and home fixture as the
//! embedded corpus.

use super::*;
use plakous_host_protocol::wire::{ClientFrame, Hello, Op, WIRE_VERSION, encode_line};
use std::io::{BufRead as _, BufReader};
use std::os::unix::net::UnixStream;

mod build_notice;
mod lifecycle;

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
        self.request(Op::ListSessions {
            workspace: None,
            runner: None,
            include_archived: false,
        })["ok"]["rows"]
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
fn a_host_tui_fixture_reaps_its_host_and_daemon_on_drop_and_panic() {
    for panic in [false, true] {
        let root = Arc::new(Mutex::new(None));
        let observed = Arc::clone(&root);
        let result = std::panic::catch_unwind(move || {
            let fixture = Fixture::with_mode(&json!({"version": 1, "steps": []}), Mode::Host);
            fixture.ensure_host();
            let run = fixture.home().join(format!(
                "host/runner/v{}/run",
                plakous::runner::protocol::PROTOCOL_VERSION
            ));
            // The host can announce before its deferred daemon start finishes.
            let until = Instant::now() + WAIT;
            let daemon = loop {
                if run.join("runner.sock").exists()
                    && let Ok(pid) = std::fs::read_to_string(run.join("runner.lock"))
                    && let Ok(pid) = pid.parse::<u32>()
                {
                    break pid;
                }
                assert!(Instant::now() < until, "daemon pid readiness timed out");
                std::thread::sleep(Duration::from_millis(5));
            };
            // Other locks are coordination fences, not daemon pid records.
            std::fs::File::create_new(run.join("fixture-empty.lock")).unwrap();
            let pids = vec![
                fixture.host.lock().unwrap().as_ref().unwrap().child.id(),
                daemon,
            ];
            *observed.lock().unwrap() = Some((fixture.root.clone(), pids));
            assert!(fixture.home().join("host/runner").is_dir());
            assert!(!panic, "fixture failure after readiness");
        });
        assert_eq!(result.is_err(), panic);
        let observed = root.lock().unwrap();
        let (path, pids) = observed.as_ref().unwrap();
        assert!(!path.exists());
        for pid in pids {
            let state = Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .unwrap();
            let state = String::from_utf8_lossy(&state.stdout);
            assert!(
                state.trim().is_empty() || state.trim().starts_with('Z'),
                "fixture process {pid} still runs: {state}"
            );
        }
    }
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
        "this session runs in plak host{build}; restarting the host moves it onto the installed build (restart its user unit, or send SIGTERM for planned handoff and start plak host again)"
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
    terminal.send(b"/new\r");
    terminal.wait_absent("socket turn answered");
    terminal.send(b"third host turn\r");
    terminal.wait_screen("socket turn answered");
    let rows = directory.rows();
    let distinct = rows
        .iter()
        .map(|row| row["session"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        distinct.len(),
        3,
        "successive /new acts create distinct sessions"
    );
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
            || frame.contains("⊘ interrupted")
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
fn a_host_tui_reload_keeps_a_running_daemon_job() {
    let fixture = Fixture::with_mode(
        &json!({
            "version": 1,
            "process_tools": true,
            "repeat_last_step": true,
            "steps": [
                {"output": {"tool": {"name": "process_session_start", "arguments": {
                    "command": "echo $$ $PPID > job.pid; read line",
                    "workdir": ".", "login": false, "tty": false,
                    "keep_stdin_open": true, "yield_time_ms": 0,
                    "max_output_bytes": 8192
                }}}, "finish": "tool_calls"},
                {"output": {"text": "daemon job survives client reload"}}
            ]
        }),
        Mode::Host,
    );
    let installed = fixture.root.join("installed");
    std::fs::create_dir_all(&installed).unwrap();
    install_this_build(&fixture, &installed);
    let execs = fixture.root.join("client-execs");
    write_script_file(
        &installed.join("plak"),
        &format!(
            "#!/bin/bash\nif [ \"$1\" != version ]; then echo client >> '{}'; fi\nexec -a plak '{}' \"$@\"\n",
            execs.display(),
            fixture.root.join("real-bin/plak").display(),
        ),
    );
    let mut terminal = Terminal::spawn(&fixture, Launch::OnPath(&installed));
    terminal.wait_ready();
    terminal.send(b"start a daemon job\r");
    terminal.wait_screen("daemon job survives client reload");
    terminal.wait_absent("Esc stop");
    terminal.send(b"/ps\r");
    terminal.wait_screen("read line");
    terminal.send(b"\x1b");
    let job = std::fs::read_to_string(fixture.workspace().join("job.pid")).unwrap();
    let (pid, parent) = job.trim().split_once(' ').unwrap();
    let pid: i32 = pid.parse().unwrap();
    let parent: u32 = parent.parse().unwrap();
    assert_ne!(parent, terminal.pid());
    assert_ne!(
        parent,
        fixture.host.lock().unwrap().as_ref().unwrap().child.id()
    );
    assert_eq!(std::fs::read_to_string(&execs).unwrap().lines().count(), 1);
    let client_pid = terminal.pid();
    assert_client_reload_keeps_host(&fixture, &mut terminal);
    assert_eq!(terminal.pid(), client_pid, "exec retains the client pid");
    terminal.send(b"/ps\r");
    let screen = terminal.wait_screen("read line");
    assert!(
        !screen.contains("outcome unknown") && !screen.contains("stopped"),
        "{screen}"
    );
    assert_eq!(std::fs::read_to_string(&execs).unwrap().lines().count(), 2);
    assert_eq!(
        std::fs::read_to_string(fixture.workspace().join("job.pid")).unwrap(),
        job
    );
    assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok());
    terminal.send(b"\x1b");
    terminal.quit();
}

#[test]
fn a_host_tui_shows_a_persistent_notice_only_when_the_installed_build_differs() {
    for mismatch in [false, true] {
        let fixture =
            Fixture::with_mode(&tool_then_answer("notice does not block work"), Mode::Host);
        let (image, host) = build_notice::installed_client(&fixture);
        build_notice::start_image_host(&fixture, &image);
        let installed_id = if mismatch { "000000000000" } else { &host };
        let key = fixture.cli(&["version", "--state-key"]);
        assert!(key.status.success());
        let key = String::from_utf8(key.stdout).unwrap();
        let probe = json!({
            "state_key": key.trim(), "export_versions": [11, 12],
            "config_error": null, "binary_id": installed_id
        });
        let installed = fixture.root.join("installed");
        std::fs::create_dir_all(&installed).unwrap();
        write_script_file(
            &installed.join("plak"),
            &format!(
                "#!/bin/bash\nif [ \"$1\" = version ] && [ \"$2\" = --reload-probe ]; then printf '%s\\n' '{}'; else exec -a plak '{}' \"$@\"; fi\n",
                probe,
                fixture.root.join("real-bin/plak").display(),
            ),
        );
        let mut terminal = Terminal::spawn(&fixture, Launch::OnPath(&installed));
        terminal.wait_ready();
        assert_eq!(Directory::connect(&fixture).binary, host);
        terminal.send(b"work with this host\r");
        if mismatch {
            terminal.wait_screen("differs from installed build");
        }
        let screen = terminal.wait_screen("notice does not block work");
        assert_eq!(
            screen.contains("differs from installed build"),
            mismatch,
            "{screen}"
        );
        if mismatch {
            for expected in [
                host.as_str(),
                installed_id,
                "restart the host unit",
                "`plak host stop`",
            ] {
                assert!(screen.contains(expected), "{screen}");
            }
        }
        terminal.quit();
    }
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
    terminal.wait_screen("host transport ended:");
    fixture.ensure_host();
    terminal.send(b"/attach\r");
    terminal.wait_ready();
    terminal.wait_screen("host transcript survives");
    terminal.quit();
}

#[test]
fn a_host_tui_refuses_a_handed_off_embedded_reload_without_a_continuation() {
    handed_off_embedded_reload(false);
}

#[test]
fn an_unknown_installed_build_refuses_before_a_later_host_handoff() {
    assert!(handed_off_embedded_reload(true) > 0);
}

fn handed_off_embedded_reload(first_probe_unknown: bool) -> usize {
    let mut fixture =
        Fixture::with_mode(&tool_then_answer("answer before the host"), Mode::Embedded);
    enable_reload_requests(&fixture);
    let (image, binary) = build_notice::installed_client(&fixture);
    let installed = fixture.root.join("handoff-install");
    std::fs::create_dir_all(&installed).unwrap();
    if first_probe_unknown {
        build_notice::make_first_installed_probe_unknown(&fixture, &image, &installed);
    }
    let mut terminal = Terminal::spawn(&fixture, Launch::OnPath(&installed));
    terminal.wait_ready();
    terminal.send(b"answer before becoming a client\r");
    terminal.wait_screen("answer before the host");
    terminal.wait_absent("Esc stop");
    let session = fixture.session_ids()[0].clone();
    let paid = provider_dispatches(&fixture);
    fixture.mode = Mode::Host;
    build_notice::start_image_host(&fixture, &image);
    let until = Instant::now() + WAIT;
    let mut unknown_probes = 0;
    loop {
        let outcome = ask_reload_when_announced(&fixture, &session, &binary);
        let written = wait_for_outcome(&outcome);
        eprintln!("embedded reload outcome: {written}");
        assert_eq!(written["outcome"], "refused", "{written}");
        let reason = written["reason"].as_str().unwrap();
        if reason.contains("the session now runs in plak host") {
            break;
        }
        // A bounded installed-build probe can legitimately answer without an
        // identity (or miss its deadline) under load. That request must refuse
        // before any handoff. Retry only this guard to reach the boundary this
        // test exercises; every other refusal remains an unexpected failure.
        assert_eq!(
            reason,
            format!("the installed build is unknown, not {binary}"),
            "{written}"
        );
        assert!(
            Instant::now() < until,
            "no known installed probe: {written}"
        );
        assert!(terminal.is_alive());
        assert_eq!(provider_dispatches(&fixture), paid);
        unknown_probes += 1;
    }
    terminal.wait_screen("requested reload refused: the session now runs in plak host");
    assert_eq!(
        provider_dispatches(&fixture),
        paid,
        "no host-authored continuation"
    );
    let mut directory = Directory::connect(&fixture);
    let rows = directory.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["residency"], "resident");
    terminal.quit();
    unknown_probes
}

#[test]
fn a_host_tui_names_the_embedded_holder_and_keeps_its_previous_attachment() {
    let mut fixture = Fixture::with_mode(
        &tool_then_answer("answer held by the first terminal"),
        Mode::Embedded,
    );
    let mut owner = Terminal::spawn_ready(&fixture);
    owner.send(b"the embedded owner\r");
    owner.wait_screen("answer held by the first terminal");
    owner.wait_absent("Esc stop");
    let held = fixture.session_ids()[0].clone();
    let pid = owner.pid().to_string();
    fixture.mode = Mode::Host;
    fixture.ensure_host();
    let mut refused = Terminal::spawn(&fixture, Launch::Resume(&held));
    let refusal = refused.wait_screen(&pid);
    assert!(refusal.contains("tree_hosted_elsewhere"));
    assert!(!refusal.contains("product host disconnected"));
    assert!(!refused.finish().success());
    let mut client = Terminal::spawn_ready(&fixture);
    client.send(b"the client session\r");
    client.wait_screen("answer held by the first terminal");
    client.send(format!("/resume {held}\r").as_bytes());
    let screen = client.wait_screen(&pid);
    assert!(screen.contains("resume refused") && !screen.contains("product host disconnected"));
    assert!(client.is_alive());
    client.send(b"/new\r");
    client.wait_absent("the client session");
    client.quit();
    owner.quit();
}

#[test]
fn a_client_reload_clears_an_inherited_epoch_handoff_environment() {
    let fixture = Fixture::with_mode(&tool_then_answer("client only epoch"), Mode::Host);
    let installed = fixture.root.join("epoch-install");
    std::fs::create_dir_all(&installed).unwrap();
    let mut terminal = Terminal::spawn(
        &fixture,
        Launch::OnPathWithEnv(
            &installed,
            &[("PLAK_RELOAD_APPLY_ORCHESTRATION", "must-be-cleared")],
        ),
    );
    terminal.wait_ready();
    fixture.wait_for_identity_cache();
    let marker = fixture.root.join("epoch-after-exec");
    write_script_file(
        &installed.join("plak"),
        &format!(
            "#!/bin/sh\nif [ \"$1\" != version ]; then printf '%s\\n' \"${{PLAK_RELOAD_APPLY_ORCHESTRATION-unset}}\" >> '{}'; fi\nexec '{}' \"$@\"\n",
            marker.display(),
            env!("CARGO_BIN_EXE_plak")
        ),
    );
    assert_client_reload_keeps_host(&fixture, &mut terminal);
    let until = Instant::now() + WAIT;
    let recorded = loop {
        let recorded = std::fs::read_to_string(&marker).unwrap_or_default();
        if !recorded.is_empty() {
            break recorded;
        }
        assert!(
            Instant::now() < until,
            "the client never exec'd the wrapper"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(recorded.lines().last(), Some("unset"));
    terminal.quit();
}

#[test]
fn an_embedded_tui_explains_that_stop_is_host_only() {
    let fixture = Fixture::embedded(
        &json!({"version": 1, "repeat_last_step": true, "steps": [{"stream": [{"text": "embedded stop work"}, {"stall": 30000}]}]}),
    );
    let mut terminal = Terminal::spawn_ready(&fixture);
    terminal.send(b"work until stopped\r");
    terminal.wait_screen("embedded stop work");
    terminal.send(b"/stop\r");
    let screen = terminal.wait_screen("/stop is a host-mode command");
    assert!(
        screen.contains("Esc stop"),
        "the hidden command did not stop embedded work"
    );
    terminal.quit();
}

#[test]
fn a_host_tui_rereads_mouse_preferences_after_the_config_editor() {
    let fixture = Fixture::with_mode(&tool_then_answer("editor round trip"), Mode::Host);
    let editor = fixture.root.join("editor");
    write_script_file(
        &editor,
        "#!/bin/sh\nprintf '\n[tui]\nmouse = false\n' >> \"$1\"\n",
    );
    let mut terminal = Terminal::spawn(
        &fixture,
        Launch::WithEnv(&[("EDITOR", editor.to_str().unwrap())]),
    );
    terminal.wait_ready();
    assert!(String::from_utf8_lossy(&terminal.raw_since(0)).contains("\x1b[?1000h"));
    let before = terminal.raw_len();
    terminal.send(b"/config\r");
    let until = Instant::now() + WAIT;
    loop {
        let raw = terminal.raw_since(before);
        if String::from_utf8_lossy(&raw).contains("\x1b[?1049h") {
            break;
        }
        assert!(Instant::now() < until, "the config editor never returned");
        std::thread::sleep(Duration::from_millis(10));
    }
    terminal.wait_ready();
    assert!(
        !String::from_utf8_lossy(&terminal.raw_since(before)).contains("\x1b[?1000h"),
        "mouse capture was enabled with stale preferences"
    );
    terminal.quit();
}

#[test]
fn a_host_tui_rebinds_without_restarting_its_host() {
    let fixture = Fixture::with_mode(&tool_then_answer("rebind stays hosted"), Mode::Host);
    let mut terminal = Terminal::spawn_ready(&fixture);
    terminal.send(b"the original host binding\r");
    terminal.wait_screen("rebind stays hosted");
    terminal.wait_absent("Esc stop");
    let mut directory = Directory::connect(&fixture);
    let before = directory.rows();
    let pid = fixture.host.lock().unwrap().as_ref().unwrap().child.id();
    terminal.send(b"/rebind\r");
    let until = Instant::now() + WAIT;
    let after = loop {
        let rows = directory.rows();
        if rows
            .iter()
            .any(|row| row["session"] != before[0]["session"] && row["residency"] == "resident")
        {
            break rows;
        }
        assert!(
            Instant::now() < until,
            "the rebind did not create its successor"
        );
        std::thread::yield_now();
    };
    assert_eq!(after.len(), before.len() + 1);
    assert_eq!(
        fixture.host.lock().unwrap().as_ref().unwrap().child.id(),
        pid
    );
    terminal.send(b"the rebound session\r");
    terminal.wait_screen("the rebound session");
    terminal.quit();
}

#[test]
fn a_host_tui_reloads_preferences_on_every_rebind() {
    for theme in ["catppuccin-latte", "catppuccin-frappe"] {
        rebind_preferences(theme);
    }
}

fn rebind_preferences(theme: &str) {
    let fixture = Fixture::with_mode(&tool_then_answer("resident preferences"), Mode::Host);
    let mut terminal = Terminal::spawn_ready(&fixture);
    terminal.send(b"keep the hosted session\r");
    terminal.wait_screen("resident preferences");
    terminal.wait_absent("Esc stop");
    let path = fixture.home().join("config.toml");
    let original = std::fs::read_to_string(&path).unwrap();
    write_private(
        &path,
        &format!("{original}\n[tui]\ntheme = \"{theme}\"\nmouse = false\n"),
    );
    let mut directory = Directory::connect(&fixture);
    let before = directory.rows().len();
    terminal.send(b"/rebind\r");
    let until = Instant::now() + WAIT;
    while directory.rows().len() == before {
        assert!(Instant::now() < until, "the rebind did not settle");
        std::thread::yield_now();
    }
    terminal.wait_screen("/ commands");
    terminal.send(b"/settings\r");
    terminal.wait_where("fresh local preferences on the hosted relaunch", |frame| {
        frame.contains("/ search settings") && frame.contains(theme)
    });
    terminal.focus_control_row("Mouse capture");
    terminal.wait_screen("now off · default on");
    terminal.send(b"\x1b");
    terminal.wait_screen("/ commands");
    terminal.quit();
}
