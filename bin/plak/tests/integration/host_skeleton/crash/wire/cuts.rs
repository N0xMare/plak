//! Every indexed host-store cut of the real-client-derived wire journeys.

use std::fmt::Write as _;

use super::*;

fn pilot(child: bool, switching: bool) -> (Vec<Commit>, Signature) {
    let fixture = fixture().unwrap();
    cold_script(&fixture, child);
    let trace = fixture.root.join("pilot.jsonl");
    let mut host = start_env(&fixture, &[], &[trace_env(&trace)]);
    let session = create(&host, &fixture).unwrap();
    replay(
        &fixture,
        &mut host,
        &session,
        &fixture_trace(child, switching),
        child,
        ReplayMode::Pilot,
    )
    .unwrap();
    host.child.as_mut().unwrap().kill().unwrap();
    host.child.take().unwrap().wait().unwrap();
    (indexed_commits(&trace), signature(&trace))
}

fn recover(fixture: &Fixture, host: &Host, session: &str, trace: &Value, child: bool) {
    let mut bindings = Bindings::new(session, &fixture.workspace());
    let mut connection = Connection::open(fixture).unwrap();
    let entries = trace["entries"].as_array().unwrap();
    let mut reconnecting = false;
    for entry in entries {
        if entry["barrier"] == "host-restarted" {
            reconnecting = true;
            continue;
        }
        if !reconnecting {
            continue;
        }
        if entry["direction"] == "client" {
            let frame = bindings.outgoing(&entry["frame"], "");
            connection.send(&frame).unwrap();
        } else if entry["direction"] == "host" {
            let actual = connection.control().unwrap();
            let expected = &entry["frame"];
            if expected["outcome"]["ok"]["answer"] == "sessions" {
                // Crash recovery may legitimately change activity/refusal facts;
                // it must still name the one original among at most two rows.
                let rows = actual["outcome"]["ok"]["rows"].as_array().unwrap();
                assert!(rows.len() <= 2, "{rows:?}");
                assert_eq!(
                    rows.iter().filter(|row| row["session"] == session).count(),
                    1
                );
            } else {
                bindings.compare(expected, &actual);
            }
        } else if entry["barrier"] == "reconnected-without-replay" {
            let (interrupts, decisions) = root_record(fixture, session);
            assert!(!interrupts.contains(&RootInterruptCause::Handoff));
            assert_eq!(decisions, 0);
        } else if entry["eof"] == "client" {
            drop(connection.writer.take());
        } else if entry["eof"] == "host" {
            connection.eof(ReplayMode::Journey).unwrap();
        }
    }
    cleanup(fixture, host, session, entries, &mut bindings, child);
}

fn cleanup(
    fixture: &Fixture,
    host: &Host,
    session: &str,
    entries: &[Value],
    bindings: &mut Bindings,
    child: bool,
) {
    let recorded = |kind| {
        entries
            .iter()
            .find(|entry| entry["direction"] == "client" && entry["frame"]["frame"] == kind)
            .unwrap()["frame"]
            .clone()
    };
    let stop = entries
        .iter()
        .find(|entry| {
            entry["direction"] == "client" && entry["frame"]["op"]["op"] == "stop_session"
        })
        .unwrap()["frame"]
        .clone();
    // Preserve the client's exact reconnect/leave ordering above. Cleanup is
    // a separate public bridge and uses the captured, unattached stop op.
    let mut connection = Connection::open(fixture).unwrap();
    connection
        .send(&bindings.outgoing(&recorded("hello"), ""))
        .unwrap();
    assert_eq!(connection.control().unwrap()["frame"], "welcome");
    stop_recovered(
        fixture,
        &mut connection,
        bindings,
        host,
        session,
        &stop,
        child,
    );
    connection.send(&recorded("bye")).unwrap();
    drop(connection.writer.take());
    connection.eof(ReplayMode::Journey).unwrap();
}

fn stop_recovered(
    fixture: &Fixture,
    connection: &mut Connection,
    bindings: &mut Bindings,
    host: &Host,
    session: &str,
    recorded: &Value,
    child: bool,
) {
    let frame = bindings.outgoing(recorded, "");
    connection.send(&frame).unwrap();
    loop {
        let response = connection.control().unwrap();
        if response["id"] == frame["id"] {
            assert_eq!(response["outcome"]["ok"]["answer"], "stopped");
            break;
        }
    }
    let until = Instant::now() + WAIT;
    let row = loop {
        let row = review_rows(host)
            .into_iter()
            .find(|row| row.session == session)
            .unwrap();
        if matches!(
            row.activity,
            SessionActivity::Stopped | SessionActivity::Idle
        ) && row.live_work.children == 0
            && row.live_work.held_children == 0
            && row.live_work.processes == 0
        {
            break row;
        }
        assert!(
            Instant::now() < until,
            "the durable stop never settled in the directory: {row:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    if child && child_dispatches(fixture) == 0 {
        use plakous_recursive_subagents::tree::TreeStore as _;
        let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
        let store =
            plakous_recursive_subagents_store::SqliteTreeStore::open(&paths.subagents_database())
                .unwrap();
        let tree = store.load(&row.node.tree_id).unwrap().unwrap();
        let tree = tree.rows();
        assert!(tree.nodes().values().all(|node| !node.active_turn));
        // A cut before any child admission leaves the pristine root-only tree
        // live: stopping that dormant session need not invent a child stop.
        if tree.nodes().len() != 1 {
            wait_stopped_tree(fixture, host, session, true);
        }
    } else {
        wait_stopped_tree(fixture, host, session, child);
    }
}

fn cut_case(
    child: bool,
    switching: bool,
    store: &str,
    ordinal: u64,
    expected: &Signature,
) -> Value {
    let fixture = fixture().unwrap();
    cold_script(&fixture, child);
    let trace = fixture.root.join("cut.jsonl");
    let stimulus = fixture_trace(child, switching);
    if let Some(mut host) = try_start(&fixture, &[], &cut_env(&trace, store, ordinal)) {
        if let Some(session) = create(&host, &fixture) {
            let result = replay(
                &fixture,
                &mut host,
                &session,
                &stimulus,
                child,
                ReplayMode::Cut,
            );
            if let Err(error) = result {
                assert!(
                    matches!(
                        error.kind(),
                        io::ErrorKind::UnexpectedEof
                            | io::ErrorKind::BrokenPipe
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::ConnectionAborted
                    ),
                    "{error}"
                );
            }
        }
        let until = Instant::now() + WAIT;
        while host.child.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < until,
                "cut {store}:{ordinal} was not reached"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!host.child.take().unwrap().wait().unwrap().success());
    }
    let actual = signature(&trace);
    assert!(
        actual
            .iter()
            .any(|(seen, index, _)| seen == store && *index == ordinal),
        "missing {store}:{ordinal}"
    );
    for point in &actual {
        assert!(
            expected.contains(point),
            "cut {store}:{ordinal}: unexpected indexed label {point:?}"
        );
    }
    for store in actual
        .iter()
        .map(|(store, _, _)| store)
        .collect::<std::collections::BTreeSet<_>>()
    {
        let observed = actual
            .iter()
            .filter(|(seen, _, _)| seen == store)
            .collect::<Vec<_>>();
        let last = observed.last().unwrap().1;
        let prefix = expected
            .iter()
            .filter(|(seen, index, _)| seen == store && *index <= last)
            .collect::<Vec<_>>();
        assert_eq!(observed, prefix, "complete reached prefix for {store}");
    }
    let before = (dispatches(&fixture), child_dispatches(&fixture));
    let mut restarted = start(&fixture);
    let session = create(&restarted, &fixture).unwrap();
    recover(&fixture, &restarted, &session, &stimulus, child);
    assert_eq!(
        before,
        (dispatches(&fixture), child_dispatches(&fixture)),
        "cut {store}:{ordinal}: replay dispatched paid work"
    );
    stop(&mut restarted);
    json!({"shape": if switching { "wire_host_restart_switch" } else { "wire_host_restart" }, "fixture": stimulus["fixture"], "child": child, "store": store,
        "cut": ordinal, "signature": actual, "dispatches_before_restart": before.0,
        "child_dispatches_before_restart": before.1, "invariants": true})
}

fn matrix(switching: bool) {
    let evidence = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/crash-sweep")
        .join(format!(
            "wire-client-{}-{}.jsonl",
            if switching { "switch" } else { "stop" },
            std::process::id()
        ));
    std::fs::create_dir_all(evidence.parent().unwrap()).unwrap();
    let mut table = String::new();
    for child in [false, true] {
        let (commits, expected) = pilot(child, switching);
        assert_eq!(
            expected,
            pilot(child, switching).1,
            "twin complete indexed signatures"
        );
        for (_, store, ordinal) in commits {
            eprintln!("wire cut switching={switching} child={child} {store}:{ordinal}");
            let row = cut_case(child, switching, &store, ordinal, &expected);
            writeln!(table, "{row}").unwrap();
            std::fs::write(&evidence, &table).unwrap();
        }
    }
    eprintln!(
        "wire client matrix: {} cuts; {}",
        table.lines().count(),
        evidence.display()
    );
}

#[test]
fn wire_host_restart() {
    matrix(false);
}

#[test]
fn wire_host_restart_switch() {
    matrix(true);
}

#[cfg(feature = "tui")]
#[test]
fn wire_host_restart_pilots_match_original_terminal_indexed_signatures() {
    for child in [false, true] {
        let wire_stop = pilot(child, false).1;
        let terminal_stop = tui::pilot(child).1;
        assert_eq!(wire_stop, terminal_stop, "stop pilot child={child}");
        let wire_switch = pilot(child, true).1;
        let terminal_switch = tui::review_pilot(child).1;
        assert_eq!(wire_switch, terminal_switch, "switch pilot child={child}");
    }
}
