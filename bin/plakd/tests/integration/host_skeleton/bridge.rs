//! The real stdio bridge carries the client's handshake and detaches without custody.

use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::time::{Duration, Instant};

use super::*;

mod client_fixtures;
mod refusals;

pub(super) fn output_after_hello(
    command: &mut std::process::Command,
    proto: u32,
) -> std::process::Output {
    let mut child = Child::spawn(
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    );
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            encode_line(&ClientFrame::<PlakExtension>::Hello(Hello {
                proto,
                binary_id: "bridge-probe".into(),
                caps: Vec::new(),
            }))
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
    finished_output(child)
}

fn finished_output(mut child: Child) -> std::process::Output {
    let status = child.wait().unwrap();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut stderr)
        .unwrap();
    std::process::Output {
        status,
        stdout,
        stderr,
    }
}

struct Bridge {
    child: Child,
    input: Option<std::process::ChildStdin>,
    lines: mpsc::Receiver<String>,
}

impl Bridge {
    fn start(fixture: &Fixture) -> Self {
        Self::configured(fixture, false, &[])
    }

    fn configured(fixture: &Fixture, auto_start: bool, environment: &[(&str, &str)]) -> Self {
        let mut command = fixture.command();
        command
            .args(["host", "connect", "--stdio"])
            .envs(environment.iter().copied());
        if auto_start {
            command.arg("--auto-start");
        }
        let mut child = Child::spawn(
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        );
        let output = child.stdout.take().unwrap();
        let (send, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines().map_while(Result::ok) {
                if send.send(line).is_err() {
                    return;
                }
            }
        });
        Self {
            input: child.stdin.take(),
            child,
            lines,
        }
    }

    fn send(&mut self, frame: &ClientFrame<PlakExtension>) {
        self.input
            .as_mut()
            .unwrap()
            .write_all(encode_line(frame).unwrap().as_bytes())
            .unwrap();
    }

    fn next(&self) -> HostFrame<PlakExtension> {
        loop {
            let line = self.lines.recv_timeout(WAIT).expect("bridge frame");
            if let Some(frame) = crate::protocol_13::decode(&line)
                .unwrap_or_else(|error| panic!("stdout was not protocol-only: {error}: {line}"))
            {
                return frame;
            }
        }
    }

    fn request(&mut self, id: RequestId, op: Op<PlakExtension>) -> Outcome<PlakExtension> {
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

    fn hello(&mut self, proto: u32) {
        self.send(&ClientFrame::Hello(Hello {
            proto,
            binary_id: "bridge-fixture".into(),
            caps: Vec::new(),
        }));
    }

    fn finish(mut self) {
        drop(self.input.take());
        let status = self.child.wait().unwrap();
        let mut diagnostics = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut diagnostics)
            .unwrap();
        assert!(status.success(), "{diagnostics}");
    }
}

/// Owns only the serving process proved by this disposable home's held guard.
struct StartedHome(PathBuf);

impl StartedHome {
    fn pid(&self) -> Option<rustix::process::Pid> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.0.join("host/host.lock"))
            .ok()?;
        if !matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock)) {
            return None;
        }
        let line = std::fs::read_to_string(self.0.join("host/host.lock")).ok()?;
        rustix::process::Pid::from_raw(line.split_whitespace().next()?.parse().ok()?)
    }

    fn host(&self) -> Host {
        let socket = std::fs::read_dir(self.0.join("host/run"))
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "sock")
            })
            .unwrap();
        Host {
            child: None,
            socket,
            lines: mpsc::channel().1,
        }
    }
}

impl Drop for StartedHome {
    fn drop(&mut self) {
        if let Some(pid) = self.pid() {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
            let until = Instant::now() + WAIT;
            while self.pid() == Some(pid) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
            }
            if self.pid() == Some(pid) {
                let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
            }
        }
        let until = Instant::now() + WAIT;
        while Instant::now() < until {
            let Ok(log) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(self.0.join("host/auto-start.log"))
            else {
                break;
            };
            if !matches!(log.try_lock(), Err(std::fs::TryLockError::WouldBlock)) {
                break;
            }
            // A released service guard precedes process exit. Its log owner
            // must also drain stderr and reap it before this fixture ends.
            std::thread::sleep(Duration::from_millis(10));
        }
        if !std::thread::panicking() {
            let path = self.0.join("host/auto-start.log");
            let remaining = opened_by(&path);
            assert!(
                remaining.is_empty(),
                "detached log owner did not reap its host; log: {}",
                std::fs::read_to_string(path).unwrap_or_default()
            );
        }
    }
}

#[test]
#[cfg(target_os = "linux")]
fn a_curated_host_keeps_os_identity_bus_and_ca_inputs_but_not_proxy_variables() {
    let Some(fixture) = fixture() else { return };
    let started = StartedHome(fixture.home());
    let preserved = [
        ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/fixture/bus"),
        ("USER", "fixture-user"),
        ("LOGNAME", "fixture-login"),
        ("SSL_CERT_FILE", "/fixture/ca.pem"),
        ("SSL_CERT_DIR", "/fixture/certs"),
    ];
    let proxies = [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "no_proxy",
        "all_proxy",
    ];
    let mut inputs = preserved.to_vec();
    inputs.extend(proxies.iter().map(|&name| (name, "excluded-fixture-proxy")));
    let mut bridge = Bridge::configured(&fixture, true, &inputs);
    bridge.hello(WIRE_VERSION);
    assert!(matches!(bridge.next(), HostFrame::Welcome(_)));
    let pid = started.pid().unwrap().as_raw_nonzero().get();
    let environment = std::fs::read(format!("/proc/{pid}/environ")).unwrap();
    let fields: Vec<_> = environment.split(|&byte| byte == 0).collect();
    for (name, value) in preserved {
        assert!(
            fields.contains(&format!("{name}={value}").as_bytes()),
            "missing {name}"
        );
    }
    for name in proxies {
        assert!(
            !fields
                .iter()
                .any(|field| field.starts_with(format!("{name}=").as_bytes())),
            "inherited {name}"
        );
    }
    bridge.finish();
}

#[test]
fn a_log_lock_without_a_host_retries_past_the_discovery_window() {
    let Some(fixture) = fixture() else { return };
    let started = StartedHome(fixture.home());
    let directory = fixture.home().join("host");
    std::fs::create_dir_all(&directory).unwrap();
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(directory.join("auto-start.log"))
        .unwrap();
    log.set_permissions(std::fs::Permissions::from_mode(0o600))
        .unwrap();
    log.lock().unwrap();
    let blocked = fixture.home().join("start-blocked");
    let mut bridge = Bridge::configured(
        &fixture,
        true,
        &[
            ("PLAK_TEST_BRIDGE_DISCOVERY_SECONDS", "1"),
            ("PLAK_TEST_BRIDGE_START_BLOCKED", blocked.to_str().unwrap()),
        ],
    );
    bridge.hello(WIRE_VERSION);
    let deadline = Instant::now() + WAIT;
    while !blocked.is_file() {
        assert!(Instant::now() < deadline, "bridge never attempted startup");
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(1100));
    assert!(
        bridge.child.try_wait().unwrap().is_none(),
        "log holder is not a ready host"
    );
    assert!(started.pid().is_none());
    drop(log);
    assert!(matches!(bridge.next(), HostFrame::Welcome(_)));
    bridge.finish();
    assert!(started.pid().is_some());
}

#[test]
fn an_explicit_local_bridge_start_survives_its_callers_process_group_signal() {
    let Some(fixture) = fixture() else { return };
    let started = StartedHome(fixture.home());
    let mut bridge = Bridge::configured(&fixture, true, &[]);
    bridge.hello(WIRE_VERSION);
    assert!(matches!(bridge.next(), HostFrame::Welcome(_)));
    let host_pid = started.pid().expect("a kernel-held host guard");
    let bridge_pid = rustix::process::Pid::from_raw(bridge.child.id().cast_signed()).unwrap();
    rustix::process::kill_process_group(bridge_pid, rustix::process::Signal::INT).unwrap();
    assert!(!bridge.child.wait().unwrap().success());
    assert_eq!(started.pid(), Some(host_pid));
    let host = started.host();
    let mut reconnect = Client::connect(&host);
    assert_eq!(reconnect.rows(1), []);
    drop(reconnect);
    let log = std::fs::metadata(fixture.home().join("host/auto-start.log")).unwrap();
    assert_eq!(log.permissions().mode() & 0o777, 0o600);
    assert!(log.len() <= 1024 * 1024);
}

#[test]
fn concurrent_bridge_and_direct_starters_reach_one_host_generation() {
    let Some(fixture) = fixture() else { return };
    let started = StartedHome(fixture.home());
    let mut first = Bridge::configured(&fixture, true, &[]);
    let mut second = Bridge::configured(&fixture, true, &[]);
    let mut direct = try_start(&fixture, &[], &[]);
    first.hello(WIRE_VERSION);
    second.hello(WIRE_VERSION);
    let HostFrame::Welcome(first_welcome) = first.next() else {
        panic!("first starter refused")
    };
    let HostFrame::Welcome(second_welcome) = second.next() else {
        panic!("second starter refused")
    };
    assert_eq!(
        first_welcome.host_generation,
        second_welcome.host_generation
    );
    if let Some(host) = &direct {
        assert_eq!(
            started.pid().unwrap().as_raw_pid(),
            host.child.as_ref().unwrap().id().cast_signed()
        );
    }
    assert_eq!(
        first.request(
            1,
            Op::ListSessions {
                workspace: None,
                runner: None,
                include_archived: false
            }
        ),
        Outcome::Ok(Answer::Sessions { rows: Vec::new() })
    );
    first.finish();
    second.finish();
    drop(started);
    if let Some(host) = &mut direct {
        assert!(host.child.as_mut().unwrap().wait().unwrap().success());
    }
}

#[test]
fn a_remote_or_unit_managed_bridge_never_starts_an_unmanaged_host() {
    let Some(fixture) = fixture() else { return };
    for unit in [None, Some("plakd-host.service"), Some("plak-host.service")] {
        let config = fixture.root.join("unit-config");
        if let Some(unit) = unit {
            std::fs::create_dir_all(config.join("systemd/user")).unwrap();
            std::fs::write(config.join("systemd/user").join(unit), "[Service]\n").unwrap();
        }
        let mut command = fixture.command();
        command
            .args(["host", "connect", "--stdio", "--auto-start"])
            .env("XDG_CONFIG_HOME", &config)
            .stdin(Stdio::null());
        if unit.is_none() {
            command.env("SSH_CONNECTION", "fixture remote session");
        }
        let output = output_after_hello(&mut command, WIRE_VERSION);
        assert!(!output.status.success());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
            json!({"frame":"refused","kind":"bridge_unavailable","reason":"start_refused"})
        );
        let message = String::from_utf8_lossy(&output.stderr);
        assert!(
            message.contains(if unit.is_some() {
                "systemctl --user start"
            } else {
                "local only"
            }),
            "{message}"
        );
        assert!(!fixture.home().join("host/host.lock").exists());
        if let Some(unit) = unit {
            assert!(message.contains(unit), "{message}");
            std::fs::remove_file(config.join("systemd/user").join(unit)).unwrap();
        }
    }
}

#[test]
fn a_losing_starter_settles_before_its_parent_returns() {
    let Some(fixture) = fixture() else { return };
    let started = StartedHome(fixture.home());
    let mut host = start(&fixture);
    let output = fixture
        .command()
        .args(["host", "__start-auto"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, [1]);
    stop(&mut host);
    // No delayed helper may claim the home after the winner was stopped.
    drop(started);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.home().join("host/host.lock"))
        .unwrap();
    assert!(file.try_lock().is_ok());
}

#[test]
fn a_foreground_host_refuses_provider_key_variables() {
    let Some(fixture) = fixture() else { return };
    let refused = fixture
        .command()
        .arg("host")
        .env("OPENAI_API_KEY", "synthetic-host-only-placeholder")
        .output()
        .unwrap();
    assert!(!refused.status.success());
    let error = String::from_utf8(refused.stderr).unwrap();
    assert!(
        error.contains("refuses provider key environment variables: OPENAI_API_KEY"),
        "{error}"
    );
    assert!(!error.contains("synthetic-host-only-placeholder"));
}

#[test]
fn an_auto_started_host_exits_after_its_idle_window_but_a_foreground_host_persists() {
    let Some(fixture) = fixture() else { return };
    let Some(foreground_fixture) = super::fixture() else {
        return;
    };
    // The test-support window replaces the production 300 s, which the pure
    // idle-clock test in `host/idle.rs` keeps. The foreground host gets the
    // same short window, so its persistence is not the window's length.
    let idle = "PLAK_TEST_HOST_IDLE_EXIT_SECONDS";
    let window = Duration::from_secs(5);
    let seconds = window.as_secs().to_string();
    let mut foreground = start_env(&foreground_fixture, &[], &[(idle, seconds.clone())]);
    let started = StartedHome(fixture.home());
    let mut bridge = Bridge::configured(&fixture, true, &[(idle, &seconds)]);
    bridge.hello(WIRE_VERSION);
    assert!(matches!(bridge.next(), HostFrame::Welcome(_)));
    assert!(started.pid().is_some());
    bridge.finish();
    let empty = Instant::now();
    while started.pid().is_some() {
        assert!(
            empty.elapsed() < window + Duration::from_secs(60),
            "auto-started host did not idle-exit"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        empty.elapsed() + Duration::from_secs(1) >= window,
        "host exited before the idle window"
    );
    assert!(
        foreground
            .child
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_none()
    );
    let mut client = Client::connect(&foreground);
    assert_eq!(client.rows(1), []);
    drop(client);
    stop(&mut foreground);
}

#[test]
fn the_stdio_bridge_relays_the_client_handshake_and_leaves_a_busy_root_running_on_eof() {
    let Some(fixture) = fixture() else { return };
    fixture.write_script(
        &json!({"version":1,"steps":[{"stream":[{"text":ANSWER},{"stall":300_000}]}]}),
    );
    let mut host = start(&fixture);
    let mut bridge = Bridge::start(&fixture);
    bridge.hello(WIRE_VERSION);
    assert!(matches!(bridge.next(), HostFrame::Welcome(welcome) if welcome.session.is_none()));
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        bridge.request(1, new_session(&fixture.workspace()))
    else {
        panic!("no created session")
    };
    let Outcome::Ok(Answer::Attached { attachment }) =
        bridge.request(2, Op::Attach { node: node.clone() })
    else {
        panic!("not attached")
    };
    assert_eq!(
        bridge.request(
            3,
            Op::Intent {
                client_id: Some(uuid::Uuid::new_v4().to_string().parse().unwrap()),
                attachment,
                intent: SessionIntent::Prompt(Draft::from("one bridged turn")),
                blobs: Vec::new(),
            }
        ),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    loop {
        if let HostFrame::Event { observation, .. } = bridge.next()
            && format!("{observation:?}").contains(ANSWER)
        {
            break;
        }
    }
    bridge.finish();
    let mut reconnect = Client::connect(&host);
    let rows = reconnect.rows(1);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].session, session);
    assert_eq!(rows[0].activity, SessionActivity::Busy);
    reconnect.attach(2, &node);
    assert_eq!(
        crash::dispatches(&fixture),
        1,
        "bridge EOF redispatched work"
    );
    drop(reconnect);
    stop(&mut host);
}

#[test]
fn a_stopped_session_has_one_canonical_notice_in_the_fresh_wire_baseline() {
    use plakous_host_protocol::SessionObservation;
    use plakous_host_protocol::views::transcript::{Lane, TranscriptRow, TranscriptScope};
    use plakous_hosted::runtime::RootTranscript;

    let stopped = |row: &TranscriptRow| matches!(row, TranscriptRow::Notice(notice) if notice.text.starts_with("stopped:"));
    let Some(fixture) = fixture() else { return };
    fixture.write_script(&json!({"version": 1, "steps": [{"output": {"text": ANSWER}}]}));
    let mut host = start(&fixture);
    let mut bridge = Bridge::start(&fixture);
    bridge.hello(WIRE_VERSION);
    assert!(matches!(bridge.next(), HostFrame::Welcome(_)));
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        bridge.request(1, new_session(&fixture.workspace()))
    else {
        panic!("no created session");
    };
    let Outcome::Ok(Answer::Attached { attachment }) = bridge.request(2, Op::Attach { node })
    else {
        panic!("not attached");
    };
    assert_eq!(
        bridge.request(
            3,
            Op::Intent {
                attachment,
                client_id: Some(uuid::Uuid::new_v4().to_string().parse().unwrap()),
                intent: SessionIntent::Prompt(Draft::from("one bridged turn")),
                blobs: Vec::new(),
            }
        ),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    let mut saw_answer = false;
    loop {
        if let HostFrame::Event { observation, .. } = bridge.next() {
            match observation {
                SessionObservation::TranscriptPatch(patch) => {
                    saw_answer |= patch
                        .upserts
                        .iter()
                        .any(|item| item.row.text() == Some(ANSWER));
                }
                SessionObservation::SetBusy(false) if saw_answer => break,
                _ => {}
            }
        }
    }

    bridge.send(&ClientFrame::Request {
        id: 4,
        op: Op::StopSession { session },
    });
    let (mut answered, mut settled) = (false, false);
    let mut live = std::collections::BTreeSet::new();
    let mut apply_stop_patch =
        |patch: plakous_host_protocol::views::transcript::TranscriptPatch| {
            for key in patch.deletes {
                live.remove(&key);
            }
            let mut canonical = false;
            for item in patch.upserts.into_iter().filter(|item| stopped(&item.row)) {
                canonical |= !RootTranscript::is_provisional(item.key);
                live.insert(item.key);
            }
            canonical
        };
    // A resync may supersede a queued patch. See the live settlement first,
    // then verify the independently rebuilt baseline.
    while !answered || !settled {
        match bridge.next() {
            HostFrame::Response { id: 4, outcome } => {
                assert_eq!(
                    outcome,
                    Outcome::Ok(Answer::Stopped {
                        runner_pending: false
                    })
                );
                answered = true;
            }
            HostFrame::Event {
                observation: SessionObservation::TranscriptPatch(patch),
                ..
            } => {
                settled |= apply_stop_patch(patch);
            }
            _ => {}
        }
    }
    bridge.send(&ClientFrame::Request {
        id: 5,
        op: Op::Intent {
            attachment,
            client_id: None,
            intent: SessionIntent::ResyncTranscript {
                scope: TranscriptScope::Root,
            },
            blobs: Vec::new(),
        },
    });
    let mut keys = Vec::new();
    loop {
        if let HostFrame::Snapshot(page) = bridge.next()
            && page.scope == TranscriptScope::Root
        {
            keys.extend(
                page.items
                    .into_iter()
                    .filter(|item| stopped(&item.row))
                    .map(|item| item.key),
            );
            if page.last {
                break;
            }
        }
    }
    assert_eq!(
        keys.len(),
        1,
        "stop notice keys in the fresh baseline: {keys:?}"
    );
    assert_eq!(keys[0].lane, Lane::Product);
    assert!(!RootTranscript::is_provisional(keys[0]));
    assert_eq!(live.into_iter().collect::<Vec<_>>(), keys);
    bridge.finish();
    stop(&mut host);
}

#[test]
fn the_stdio_bridge_does_not_consume_a_separate_handshake_or_mask_a_version_refusal() {
    const OLDER: u32 = WIRE_VERSION - 1;
    let Some(fixture) = fixture() else { return };
    let mut host = start(&fixture);
    let mut bridge = Bridge::start(&fixture);
    bridge.hello(OLDER);
    assert!(matches!(
        bridge.next(),
        HostFrame::Refused(
            plakous_host_protocol::wire::HandshakeRefusal::VersionMismatch {
                host: WIRE_VERSION,
                client: OLDER
            }
        )
    ));
    assert!(
        bridge.lines.recv_timeout(WAIT).is_err(),
        "the refused handshake received another frame"
    );
    bridge.finish();
    let mut client = Client::connect(&host);
    assert_eq!(client.rows(1), []);
    drop(client);
    stop(&mut host);
}

#[test]
fn bridge_eof_keeps_a_busy_root_and_running_child_without_transfer_or_redispatch() {
    let Some(fixture) = fixture() else { return };
    crash::client_control::script(&fixture, true);
    let mut root = spawning_script();
    root["product_child_script"] = json!(fixture.root.join("child.json"));
    root["steps"][1] = json!({
        "stream": [{"text": "bridge root waiting"}, {"stall": 300_000}]
    });
    fixture.write_script(&root);
    let child_path = fixture.root.join("child.json");
    let mut child: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&child_path).unwrap()).unwrap();
    child["steps"][0]["stream"][1]["stall"] = json!(300_000);
    std::fs::write(child_path, serde_json::to_vec(&child).unwrap()).unwrap();
    let mut host = start(&fixture);
    let mut bridge = Bridge::start(&fixture);
    bridge.hello(WIRE_VERSION);
    assert!(matches!(bridge.next(), HostFrame::Welcome(_)));
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        bridge.request(1, new_session(&fixture.workspace()))
    else {
        panic!("no session")
    };
    let Outcome::Ok(Answer::Attached { attachment }) =
        bridge.request(2, Op::Attach { node: node.clone() })
    else {
        panic!("no attachment")
    };
    assert_eq!(
        bridge.request(
            3,
            Op::Intent {
                client_id: Some(uuid::Uuid::new_v4().to_string().parse().unwrap()),
                attachment,
                intent: SessionIntent::Prompt(Draft::from("bridge busy root and child")),
                blobs: Vec::new(),
            }
        ),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    let mut observer = Client::connect(&host);
    let until = Instant::now() + WAIT;
    loop {
        let rows = observer.rows(1);
        if rows.iter().any(|row| {
            row.session == session
                && row.activity == SessionActivity::Busy
                && row.live_work.children > 0
        }) && crash::dispatches(&fixture) == 2
            && crash::client_control::child_dispatches(&fixture) == 1
        {
            break;
        }
        assert!(
            Instant::now() < until,
            "the running tree did not appear: {rows:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let before = (
        crash::dispatches(&fixture),
        crash::client_control::child_dispatches(&fixture),
    );
    let holder = custody_pid(&fixture, &node.tree_id);
    assert_eq!(holder, Some(host.child.as_ref().unwrap().id()));
    bridge.finish();
    let attachment = observer.attach(2, &node);
    assert_eq!(custody_pid(&fixture, &node.tree_id), holder);
    assert!(observer.rows(3).iter().any(|row| row.session == session
        && row.activity == SessionActivity::Busy
        && row.live_work.children > 0));
    assert_eq!(
        before,
        (
            crash::dispatches(&fixture),
            crash::client_control::child_dispatches(&fixture)
        )
    );
    let (interrupts, decisions) = root_record(&fixture, &session);
    assert!(!interrupts.contains(&RootInterruptCause::Handoff));
    assert_eq!(decisions, 0);
    assert!(attachment > 0);
    assert!(matches!(
        observer.request(4, Op::StopSession { session }),
        Outcome::Ok(Answer::Stopped { .. })
    ));
    drop(observer);
    stop(&mut host);
}

#[test]
fn the_stdio_bridge_keeps_same_user_clients_remote_for_local_only_reload_intents() {
    use plakd_wire_types::{PlakExtension, SurfaceIntent};
    let Some(fixture) = fixture() else { return };
    let mut host = start(&fixture);
    let mut bridge = Bridge::start(&fixture);
    bridge.hello(WIRE_VERSION);
    assert!(matches!(bridge.next(), HostFrame::Welcome(_)));
    let Outcome::Ok(Answer::Created { node, .. }) =
        bridge.request(1, new_session(&fixture.workspace()))
    else {
        panic!("no session")
    };
    let Outcome::Ok(Answer::Attached { attachment }) = bridge.request(2, Op::Attach { node })
    else {
        panic!("not attached")
    };
    let line = encode_line(&ClientFrame::<PlakExtension>::Request {
        id: 3,
        op: Op::Intent {
            attachment,
            client_id: None,
            blobs: Vec::new(),
            intent: SessionIntent::Extension(SurfaceIntent::CheckBuild),
        },
    })
    .unwrap();
    bridge
        .input
        .as_mut()
        .unwrap()
        .write_all(line.as_bytes())
        .unwrap();
    loop {
        if let HostFrame::Response { id: 3, outcome } = bridge.next() {
            assert!(matches!(outcome, Outcome::Refused(
                plakous_host_protocol::wire::RequestRefusal::RefusedForRole { row }
            ) if row == "local_face"));
            break;
        }
    }
    assert_eq!(crash::dispatches(&fixture), 0);
    bridge.finish();
    stop(&mut host);
}
