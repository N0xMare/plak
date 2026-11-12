//! The terminal observes host loss at each indexed host commit and opens
//! the resident session again on the restarted host, without replaying input.

use super::client_control::*;
use super::*;
use std::fmt::Write as _;

mod recording;

// Directory switching is driven by the terminal while the host owns the work.

/// [`drive`], with the terminal's own directory acts in the middle: after
/// the root's answer, or while a real child runs, the TUI makes
/// a session with `/new` (its own `new_session` and attach), switches back
/// with `/resume <id>`, then stops the original.
fn review_drive_switch(fixture: &Fixture, session: &str, cut: bool, child: bool) {
    review_drive_recorded(
        fixture,
        session,
        cut,
        child,
        &recording::Recording::default(),
    );
}

fn review_drive_recorded(
    fixture: &Fixture,
    session: &str,
    cut: bool,
    child: bool,
    recording: &recording::Recording,
) {
    let previous_connection = recording.connections();
    let mut terminal = super::super::super::offline_tui::attach_to_host(&fixture.root, session);
    let until = Instant::now() + WAIT;
    let mut step = 0;
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
        let ready = screen.contains("%/") && screen.contains("/ commands");
        match step {
            0 if ready && recording.initial_controls_acked(previous_connection) => {
                recording.mark("ready");
                terminal.send(b"crash terminal original prompt\r");
                step = 1;
            }
            1 if screen.contains(ANSWER) && (!child || child_dispatches(fixture) > 0) => {
                recording.mark("answered");
                if recording.is_recording() {
                    recording.mark("new-session-time-boundary");
                    separate_session_timestamp(fixture, session);
                }
                terminal.send(b"/new\r");
                step = 2;
            }
            2 if ready && !screen.contains(ANSWER) && !screen.contains(PREFIX) => {
                recording.mark("new-session-ready");
                terminal.send(format!("/resume {session}\r").as_bytes());
                step = 3;
            }
            3 if screen.contains(ANSWER) => {
                recording.mark("original-session-restored");
                if recording.is_recording() {
                    recording.mark("stop-time-boundary");
                    separate_stop_timestamp(fixture);
                }
                terminal.send(b"/stop\r");
                step = 4;
            }
            4 if (screen.contains("session work stopped")
                || screen.contains("stopped: the session's work ended"))
                && stopped_notice_committed(fixture, session) =>
            {
                recording.mark("stopped");
                terminal.quit();
                return;
            }
            _ => {}
        }
        assert!(
            Instant::now() < until,
            "TUI made no progress at step {step}: {screen}"
        );
        std::thread::sleep(Duration::from_millis(15));
    }
}

pub(super) fn review_pilot(child: bool) -> (Vec<Commit>, Signature) {
    let baseline = fixture().unwrap();
    script(&baseline, child);
    let trace = baseline.root.join("trace.jsonl");
    let mut host = start_env(&baseline, &[], &[trace_env(&trace)]);
    let session = create(&host, &baseline).unwrap();
    review_drive_switch(&baseline, &session, false, child);
    let rows = review_rows(&host);
    assert_eq!(rows.len(), 2, "the pilot made one more session: {rows:?}");
    wait_stopped_tree(&baseline, &host, &session, child);
    host.child.as_mut().unwrap().kill().unwrap();
    host.child.take().unwrap().wait().unwrap();
    let commits = indexed_commits(&trace);
    assert_ne!(commits.len(), 0);
    (commits, signature(&trace))
}

#[test]
fn review_tui_host_restart_switch() {
    let Some(probe) = fixture() else { return };
    drop(probe);
    let evidence = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/crash-sweep")
        .join(format!(
            "review-tui-host-restart-switch-{}.jsonl",
            std::process::id()
        ));
    std::fs::create_dir_all(evidence.parent().unwrap()).unwrap();
    let mut table = String::new();
    for child in [false, true] {
        let (commits, expected) = review_pilot(child);
        assert_eq!(expected, review_pilot(child).1, "twin pilots");
        for (_, store, ordinal) in commits {
            let fixture = fixture().unwrap();
            script(&fixture, child);
            let trace = fixture.root.join("cut.jsonl");
            if let Some(mut host) = try_start(&fixture, &[], &cut_env(&trace, &store, ordinal)) {
                if let Some(session) = create(&host, &fixture) {
                    review_drive_switch(&fixture, &session, true, child);
                }
                let status = host.child.take().unwrap().wait().unwrap();
                assert!(
                    !status.success(),
                    "cut {store}:{ordinal} did not kill the host"
                );
            }
            let actual = signature(&trace);
            let label = expected
                .iter()
                .find(|(seen_store, seen_ordinal, _)| {
                    *seen_store == store && *seen_ordinal == ordinal
                })
                .cloned();
            assert_eq!(
                actual
                    .iter()
                    .find(|(seen_store, seen_ordinal, _)| *seen_store == store
                        && *seen_ordinal == ordinal)
                    .cloned(),
                label,
                "cut {store}:{ordinal} must hit its pilot label"
            );
            let before = dispatches(&fixture);
            let children_before = child_dispatches(&fixture);
            let mut restarted = start(&fixture);
            let session = create(&restarted, &fixture).unwrap();
            let rows = review_rows(&restarted);
            assert!(
                rows.len() <= 2 && rows.iter().filter(|row| row.session == session).count() == 1,
                "cut {store}:{ordinal}: {rows:?}"
            );
            let mut terminal =
                super::super::super::offline_tui::attach_to_host(&fixture.root, &session);
            terminal.wait_ready();
            let (interrupts, decisions) = root_record(&fixture, &session);
            assert!(!interrupts.contains(&RootInterruptCause::Handoff));
            assert_eq!(
                decisions, 0,
                "cut {store}:{ordinal}: a continuation was decided"
            );
            assert_eq!(
                dispatches(&fixture),
                before,
                "cut {store}:{ordinal} replayed a turn"
            );
            assert_eq!(
                child_dispatches(&fixture),
                children_before,
                "cut {store}:{ordinal} replayed a child"
            );
            terminal.send(b"/stop\r");
            terminal.wait_where("the durable session stop", |frame| {
                frame.contains("session work stopped")
                    || frame.contains("stopped: the session's work ended")
            });
            terminal.quit();
            stop(&mut restarted);
            writeln!(
                table,
                "{}",
                json!({"shape": "review_tui_host_restart_switch", "child": child, "store": store, "cut": ordinal, "label": label.map(|label| label.2), "sessions_after_restart": rows.len(), "dispatches_before_restart": before, "child_dispatches_before_restart": children_before, "invariants": true})
            )
            .unwrap();
            std::fs::write(&evidence, &table).unwrap();
        }
    }
    eprintln!(
        "review_tui_host_restart_switch: {} cuts; {}",
        table.lines().count(),
        evidence.display()
    );
}

/// Drives a live TUI until it answers and durably stops its work, or the
/// cut ends the host. A failed handshake is plain terminal output too.
fn drive(fixture: &Fixture, session: &str, cut: bool, child: bool) {
    drive_recorded(
        fixture,
        session,
        cut,
        child,
        &recording::Recording::default(),
    );
}

fn drive_recorded(
    fixture: &Fixture,
    session: &str,
    cut: bool,
    child: bool,
    recording: &recording::Recording,
) {
    let previous_connection = recording.connections();
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
        if !prompted
            && screen.contains("%/")
            && screen.contains("/ commands")
            && recording.initial_controls_acked(previous_connection)
        {
            recording.mark("ready");
            terminal.send(b"crash terminal original prompt\r");
            prompted = true;
        }
        if !stopping && screen.contains(ANSWER) && (!child || child_dispatches(fixture) > 0) {
            recording.mark("answered");
            terminal.send(b"/stop\r");
            stopping = true;
        }
        if (screen.contains("session work stopped")
            || screen.contains("stopped: the session's work ended"))
            && stopped_notice_committed(fixture, session)
        {
            recording.mark("stopped");
            terminal.quit();
            return;
        }
        assert!(Instant::now() < until, "TUI made no progress: {screen}");
        std::thread::sleep(Duration::from_millis(15));
    }
}

/// Regenerate the stimulus from the real client, never from invented ops.
/// The private client gate compares its emitted control trace to the same
/// normalized fixture; this raw artifact retains observations for auditing.
#[test]
#[ignore = "on-demand real-client trace capture"]
fn capture_real_client_host_control_traces() {
    let output = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/client-traces");
    std::fs::create_dir_all(&output).unwrap();
    for child in [false, true] {
        for switching in [false, true] {
            let fixture = fixture().unwrap();
            cold_script(&fixture, child);
            let mut host = start(&fixture);
            let session = create(&host, &fixture).unwrap();
            let recording = recording::Recording::default();
            {
                let _proxy = recording::Proxy::start(&host.socket, recording.clone());
                if switching {
                    review_drive_recorded(&fixture, &session, false, child, &recording);
                } else {
                    drive_recorded(&fixture, &session, false, child, &recording);
                }
            }
            wait_stopped_tree(&fixture, &host, &session, child);
            let before = dispatches(&fixture);
            let children_before = if child { child_dispatches(&fixture) } else { 0 };
            host.child.as_mut().unwrap().kill().unwrap();
            host.child.take().unwrap().wait().unwrap();
            let mut host = start(&fixture);
            recording.mark("host-restarted");
            {
                let _proxy = recording::Proxy::start(&host.socket, recording.clone());
                let previous_connection = recording.connections();
                let mut terminal =
                    super::super::super::offline_tui::attach_to_host(&fixture.root, &session);
                terminal.wait_ready();
                let until = Instant::now() + WAIT;
                while !recording.initial_controls_acked(previous_connection) {
                    assert!(
                        Instant::now() < until,
                        "initial controls were not acknowledged"
                    );
                    std::thread::sleep(Duration::from_millis(15));
                }
                recording.mark("reconnected-without-replay");
                terminal.quit();
            }
            assert_eq!(dispatches(&fixture), before);
            if child {
                assert_eq!(child_dispatches(&fixture), children_before);
            }
            let (interrupts, decisions) = root_record(&fixture, &session);
            assert!(!interrupts.contains(&RootInterruptCause::Handoff));
            assert_eq!(decisions, 0);
            let entries = recording.entries();
            wire::transcript::assert_recorded(&fixture, &session, &entries);
            let document = json!({
                "fixture": format!("host-control-{}-{}", if switching { "switch" } else { "stop" }, if child { "child" } else { "root" }),
                "session": session,
                "workspace": fixture.workspace(),
                "provenance": {"client_sha256": binary_sha256(), "host_sha256": binary_sha256()},
                "entries": entries,
            });
            let path = output.join(format!(
                "{}-raw.json",
                document["fixture"].as_str().unwrap()
            ));
            std::fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
            stop(&mut host);
            eprintln!("real-client control trace: {}", path.display());
        }
    }
}

fn binary_sha256() -> &'static str {
    static DIGEST: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DIGEST.get_or_init(|| {
        plakous::model::sha256_hex(&[&std::fs::read(env!("CARGO_BIN_EXE_plak")).unwrap()])
    })
}

pub(super) fn pilot(child: bool) -> (Vec<Commit>, Signature) {
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
    wait_stopped_tree(&baseline, &host, &session, child);
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
