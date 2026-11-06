//! The terminal observes host loss at each indexed host commit and opens
//! the resident session again on the restarted host, without replaying input.

use super::*;
use plak::wire_transport::PlakExtension;
use plakous_host_protocol::client::{self, Transport};
use std::fmt::Write as _;

fn create(host: &Host, fixture: &Fixture) -> Option<String> {
    let stream = UnixStream::connect(&host.socket).ok()?;
    stream.set_read_timeout(Some(WAIT)).unwrap();
    let (send, receive, _) = client::connect_directory::<PlakExtension>(
        Transport::new(stream.try_clone().unwrap(), stream),
        Hello {
            proto: WIRE_VERSION,
            binary_id: "crash-terminal".into(),
            caps: Vec::new(),
        },
    )
    .ok()?;
    send.request_operation(Op::NewSession {
        workspace: fixture.workspace().display().to_string(),
        template: None,
        request_id: REQUEST.into(),
    })?;
    match receive.recv_response(WAIT).ok()?.1 {
        Outcome::Ok(Answer::Created { session, .. }) => Some(session),
        _ => None,
    }
}

fn script(fixture: &Fixture, child: bool) {
    if !child {
        fixture.write_script(&busy_script());
        return;
    }
    let child_script = fixture.root.join("child.json");
    std::fs::write(&child_script, json!({"version": 1, "repeat_last_step": true, "steps": [{"stream": [{"text": "child working"}, {"stall": 30000}]}]}).to_string()).unwrap();
    let mut root = spawning_script();
    root["product_child_script"] = json!(child_script);
    root["steps"].as_array_mut().unwrap().remove(1);
    fixture.write_script(&root);
    let config = fixture.home().join("config.toml");
    let text = std::fs::read_to_string(&config)
        .unwrap()
        .replace("subagents = false", "subagents = true");
    write_private(&config, &text);
}

/// Drives a live TUI until it answers and durably stops its work, or the
/// cut ends the host. A failed handshake is plain terminal output too.
fn drive(fixture: &Fixture, session: &str, cut: bool, child: bool) {
    let mut terminal = super::super::super::offline_tui::attach_to_host(&fixture.root, session);
    let until = Instant::now() + WAIT;
    let mut prompted = false;
    let mut stopping = false;
    loop {
        let screen = terminal.screen();
        if cut && (screen.contains("host disconnected") || !terminal.is_alive()) {
            assert!(screen.contains("host"), "loss must be plain: {screen}");
            if terminal.is_alive() {
                terminal.quit();
            } else {
                let _ = terminal.finish_with_screen();
            }
            return;
        }
        if !prompted && screen.contains("%/") && screen.contains("/ commands") {
            terminal.send(b"crash terminal original prompt\r");
            prompted = true;
        }
        if !stopping && screen.contains(ANSWER) && (!child || child_dispatches(fixture) > 0) {
            terminal.send(b"/stop\r");
            stopping = true;
        }
        if screen.contains("session work stopped")
            || screen.contains("stopped: the session's work ended")
        {
            terminal.quit();
            return;
        }
        assert!(Instant::now() < until, "TUI made no progress: {screen}");
        std::thread::sleep(Duration::from_millis(15));
    }
}

fn child_dispatches(fixture: &Fixture) -> usize {
    read_evidence(fixture.root.join("child-evidence.jsonl"))
        .unwrap()
        .iter()
        .filter(|record| record.kind == EvidenceKind::ProviderDispatched)
        .count()
}

type Commit = (u64, String, u64);
type Signature = Vec<(String, u64, String)>;

fn signature(trace: &Path) -> Signature {
    let mut commits = std::fs::read_to_string(trace)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|row| row["indexed"] == true)
        .map(|row| {
            (
                row["store"].as_str().unwrap().to_owned(),
                row["store_ordinal"].as_u64().unwrap(),
                row["label"].as_str().unwrap().to_owned(),
            )
        })
        .collect::<Vec<_>>();
    commits.sort();
    commits
}

fn pilot(child: bool) -> (Vec<Commit>, Signature) {
    let baseline = fixture().unwrap();
    script(&baseline, child);
    let trace = baseline.root.join("trace.jsonl");
    let mut host = start_env(&baseline, &[], &[trace_env(&trace)]);
    let session = create(&host, &baseline).unwrap();
    drive(&baseline, &session, false, child);
    if child {
        assert_eq!(
            child_dispatches(&baseline),
            1,
            "the pilot stopped a real running child"
        );
    }
    host.child.as_mut().unwrap().kill().unwrap();
    host.child.take().unwrap().wait().unwrap();
    let commits = indexed_commits(&trace);
    assert_ne!(commits.len(), 0);
    (commits, signature(&trace))
}

#[test]
fn tui_host_restart() {
    let Some(probe) = fixture() else { return };
    drop(probe);
    let evidence = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/crash-sweep")
        .join(format!("tui-host-restart-{}.jsonl", std::process::id()));
    std::fs::create_dir_all(evidence.parent().unwrap()).unwrap();
    let mut table = String::new();
    for child in [false, true] {
        let (commits, expected) = pilot(child);
        assert_eq!(
            expected,
            pilot(child).1,
            "twin pilots must have identical per-store indexed commits"
        );
        for (_, store, ordinal) in commits {
            let fixture = fixture().unwrap();
            script(&fixture, child);
            let trace = fixture.root.join("cut.jsonl");
            if let Some(mut host) = try_start(&fixture, &[], &cut_env(&trace, &store, ordinal)) {
                if let Some(session) = create(&host, &fixture) {
                    drive(&fixture, &session, true, child);
                }
                let status = host.child.take().unwrap().wait().unwrap();
                assert!(
                    !status.success(),
                    "cut {store}:{ordinal} did not kill the host"
                );
            }
            assert!(
                indexed_commits(&trace)
                    .iter()
                    .any(|(_, seen_store, seen_ordinal)| *seen_store == store
                        && *seen_ordinal == ordinal),
                "cut {store}:{ordinal} was not reached"
            );
            let actual = signature(&trace);
            assert_eq!(
                actual
                    .iter()
                    .find(|(seen_store, seen_ordinal, _)| *seen_store == store
                        && *seen_ordinal == ordinal),
                expected
                    .iter()
                    .find(|(seen_store, seen_ordinal, _)| *seen_store == store
                        && *seen_ordinal == ordinal),
                "cut {store}:{ordinal} must hit its pilot label"
            );
            let before = dispatches(&fixture);
            let children_before = child_dispatches(&fixture);
            let mut restarted = start(&fixture);
            let session = create(&restarted, &fixture).unwrap();
            let mut terminal =
                super::super::super::offline_tui::attach_to_host(&fixture.root, &session);
            terminal.wait_ready();
            let (interrupts, decisions) = root_record(&fixture, &session);
            assert!(
                !interrupts.contains(&RootInterruptCause::Handoff),
                "a client never hands a session off"
            );
            assert_eq!(decisions, 0, "no handoff continuation was decided");
            // Owner loss is held, never automatically replayed by a face.
            assert_eq!(
                dispatches(&fixture),
                before,
                "cut {store}:{ordinal} replayed a turn on re-attach"
            );
            assert_eq!(
                child_dispatches(&fixture),
                children_before,
                "cut {store}:{ordinal} replayed a child on re-attach"
            );
            terminal.send(b"/stop\r");
            terminal.wait_where("the durable session stop", |frame| {
                frame.contains("session work stopped")
                    || frame.contains("stopped: the session's work ended")
            });
            terminal.quit();
            stop(&mut restarted);
            writeln!(table, "{}", json!({"shape": "tui_host_restart", "child": child, "store": store, "cut": ordinal, "dispatches_before_restart": before, "child_dispatches_before_restart": children_before, "invariants": true})).unwrap();
            std::fs::write(&evidence, &table).unwrap();
        }
    }
    eprintln!(
        "tui_host_restart: {} cuts; {}",
        table.lines().count(),
        evidence.display()
    );
}
