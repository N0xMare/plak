//! Shared public host/store/dispatch oracles, independent of terminal stimulus.
use super::*;
use plak::wire_transport::PlakExtension;
use plakous_host_protocol::client::{self, Transport};
pub(super) type Commit = (u64, String, u64);
pub(super) type Signature = Vec<(String, u64, String)>;

/// Keep the directory's real timestamp ordering independent of UUID ties.
/// Only trace capture/replay uses this fixture barrier, never the client.
pub(super) fn separate_session_timestamp(fixture: &Fixture, session: &str) {
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let database = rusqlite::Connection::open_with_flags(
        paths.product_database(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let until = Instant::now() + WAIT;
    let mut previous = None;
    let mut stable_since = Instant::now();
    loop {
        let signature: (i64, i64) = database
            .query_row(
                "SELECT updated_at,revision FROM sessions WHERE id=?1",
                [session],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        if previous != Some(signature) {
            previous = Some(signature);
            stable_since = Instant::now();
        }
        if stable_since.elapsed() >= Duration::from_millis(1100)
            && std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                > u64::try_from(signature.0).unwrap()
        {
            break;
        }
        assert!(
            Instant::now() < until,
            "fixture checkpoint did not settle before the new-session boundary"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The next stop must sort strictly after the newly created session, not tie
/// its update second and fall back to random UUID ordering.
pub(super) fn separate_stop_timestamp(fixture: &Fixture) {
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let database = rusqlite::Connection::open_with_flags(
        paths.product_database(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let newest: i64 = database
        .query_row("SELECT MAX(updated_at) FROM sessions", [], |row| row.get(0))
        .unwrap();
    let until = Instant::now() + WAIT;
    while std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        <= u64::try_from(newest).unwrap()
    {
        assert!(
            Instant::now() < until,
            "fixture clock did not pass the directory update"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub(super) fn review_rows(host: &Host) -> Vec<SessionRow> {
    let stream = UnixStream::connect(&host.socket).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    let (send, receive, _) = client::connect_directory::<PlakExtension>(
        Transport::new(stream.try_clone().unwrap(), stream),
        Hello {
            proto: WIRE_VERSION,
            binary_id: "review".into(),
            caps: Vec::new(),
        },
    )
    .unwrap();
    send.request_operation(Op::ListSessions {
        workspace: None,
        runner: None,
        include_archived: false,
    })
    .unwrap();
    match receive.recv_response(WAIT).unwrap().1 {
        Outcome::Ok(Answer::Sessions { rows }) => rows,
        other => panic!("{other:?}"),
    }
}

pub(super) fn create(host: &Host, fixture: &Fixture) -> Option<String> {
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
        runner: plakous_host_protocol::wire::RunnerLabel::local(),
        profile: None,
        model: None,
        workspace: fixture.workspace().display().to_string(),
        template: None,
        request_id: REQUEST.into(),
    })
    .ok()?;
    let Outcome::Ok(Answer::Created { session, .. }) = receive.recv_response(WAIT).ok()?.1 else {
        return None;
    };
    // Explicit fixture metadata keeps directory answers exact while the first
    // prompt's generated title is published. Never normalize missing fields.
    send.request_operation(Op::RenameSession {
        session: session.clone(),
        title: Some("original source".to_owned()),
    })
    .ok()?;
    matches!(
        receive.recv_response(WAIT).ok()?.1,
        Outcome::Ok(Answer::Renamed { .. })
    )
    .then_some(session)
}

pub(in super::super) fn script(fixture: &Fixture, child: bool) {
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

pub(in super::super) fn child_dispatches(fixture: &Fixture) -> usize {
    read_evidence(fixture.root.join("child-evidence.jsonl"))
        .unwrap()
        .iter()
        .filter(|record| record.kind == EvidenceKind::ProviderDispatched)
        .count()
}

/// Keep identity readiness consistently cold in capture and public replay.
/// Otherwise a background image hash may race the reconnect welcome.
pub(super) fn cold_script(fixture: &Fixture, child: bool) {
    script(fixture, child);
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.script()).unwrap()).unwrap();
    document["identity_hash_delay_ms"] = json!(300_000);
    fixture.write_script(&document);
}

pub(super) fn wait_stopped_tree(fixture: &Fixture, host: &Host, session: &str, child: bool) {
    use plakous_recursive_subagents::tree::TreeStore as _;
    let until = Instant::now() + WAIT;
    if child {
        let node = review_rows(host)
            .into_iter()
            .find(|row| row.session == session)
            .unwrap()
            .node;
        let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
        let store =
            plakous_recursive_subagents_store::SqliteTreeStore::open(&paths.subagents_database())
                .unwrap();
        loop {
            let tree = store.load(&node.tree_id).unwrap().unwrap();
            let tree = tree.rows();
            if tree.state == plakous_recursive_subagents::tree::TreeAdmissionState::Interrupted
                && tree.nodes().values().all(|node| {
                    !node.active_turn
                        && (node.agent_id == tree.root_agent_id
                            || node.status
                                == plakous_recursive_subagents::protocol::NodeStatus::Interrupted
                            || node.status.is_finished())
                })
            {
                break;
            }
            assert!(
                Instant::now() < until,
                "the pilot's stopped tree never settled: {tree:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

pub(super) fn signature(trace: &Path) -> Signature {
    let mut commits = std::fs::read_to_string(trace)
        .unwrap()
        .split_inclusive('\n')
        .filter(|line| line.ends_with('\n'))
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
