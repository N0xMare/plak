//! Rollback and publication resume original handoffs without a client attachment.
use super::*;

#[test]
fn host_move_live_post_drain_rollback_continues_the_root_and_child_unattended_once() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let (mut host, session, _) = submitting_child(
        &fixture,
        &[("PLAK_TEST_HOST_UPGRADE_FAILURE", "reservation-lines".into())],
    );
    let child_release = finish_child_script(&fixture);
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let source = plak::store::session_records::SqliteSessionStore::new(paths.clone())
        .load(&session)
        .unwrap();
    let before = plak::store::tree::load_tree(&paths, &source.node.tree_id)
        .unwrap()
        .unwrap();
    let child = before
        .nodes()
        .values()
        .find(|node| node.parent_agent_id.is_some())
        .unwrap()
        .clone();
    let result = output_env(
        &fixture,
        &[
            "host",
            "upgrade",
            "--expect-binary",
            &binary(&fixture),
            "--drain",
            "0",
        ],
        true,
        &[(ENABLED, "1")],
    );
    assert!(!result.status.success());
    assert_eq!(receipts(&fixture)[0]["phase"], "aborted", "{result:?}");
    root_continued(&fixture, &session, &plak::home::state::state_key());
    std::fs::write(child_release, "submit the continued result").unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        let tree = plak::store::tree::load_tree(&paths, &source.node.tree_id)
            .unwrap()
            .unwrap();
        let after = tree.node(&child.agent_id).unwrap();
        if !after.active_turn
            && child_dispatches(&fixture) == 2
            && tree
                .attempts()
                .get(child.attempt_id.as_ref().unwrap())
                .unwrap()
                .output_json
                .is_some()
        {
            assert_eq!(after.attempt_id, child.attempt_id);
            assert_eq!(after.line_id, child.line_id);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "rollback child did not continue once: {tree:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    root_continued(&fixture, &session, &plak::home::state::state_key());
    assert_eq!(root_record(&fixture, &session).1, 1);
    assert_eq!(drain::dispatches(&fixture), 3);
    assert_eq!(child_dispatches(&fixture), 2);
    stop(&mut host);
}

#[test]
fn host_move_live_paid_compaction_at_the_bound_refuses_without_cancellation_or_redispatch() {
    drain::live_paid_fold_refusal();
}

#[test]
fn host_move_live_destination_interrupted_child_continues_once_without_an_attachment() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let (mut host, session, _) = submitting_child(&fixture, &[]);
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let source = plak::store::session_records::SqliteSessionStore::new(paths.clone())
        .load(&session)
        .unwrap();
    let original = plak::store::tree::load_tree(&paths, &source.node.tree_id)
        .unwrap()
        .unwrap();
    let child = original
        .nodes()
        .values()
        .find(|node| node.parent_agent_id.is_some())
        .unwrap()
        .clone();
    let child_release = finish_child_script(&fixture);
    let result = output_env(
        &fixture,
        &[
            "host",
            "upgrade",
            "--expect-binary",
            &binary(&fixture),
            "--drain",
            "0",
        ],
        true,
        &[(ENABLED, "1")],
    );
    assert!(result.status.success(), "{result:?}");
    root_continued(&fixture, &session, DESTINATION);
    std::fs::write(child_release, "submit the continued result").unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        let (tree, _) = plakous_recursive_subagents_store::SqliteTreeStore::inspect_move_tree(
            &fixture
                .home()
                .join("state")
                .join(DESTINATION)
                .join("subagents.sqlite3"),
            &source.node.tree_id,
        )
        .unwrap()
        .unwrap();
        let next = tree.rows().node(&child.agent_id).unwrap();
        if !next.active_turn
            && child_dispatches(&fixture) == 2
            && tree
                .rows()
                .attempts()
                .get(child.attempt_id.as_ref().unwrap())
                .unwrap()
                .output_json
                .is_some()
        {
            assert_eq!(next.attempt_id, child.attempt_id);
            assert_eq!(next.line_id, child.line_id);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "destination child did not continue once: dispatches={}, status={:?}, continuation={:?}, host={:?}",
            child_dispatches(&fixture),
            next.status,
            tree.rows().loss_continuation(&child.agent_id),
            host.lines.try_iter().collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    root_continued(&fixture, &session, DESTINATION);
    assert_eq!(drain::dispatches(&fixture), 3);
    assert_eq!(child_dispatches(&fixture), 2);
    kill(&mut host);
    let mut restarted = start_env(
        &fixture,
        &[],
        &[
            (ENABLED, "1".into()),
            ("PLAK_TEST_STATE_KEY", DESTINATION.into()),
        ],
    );
    root_continued(&fixture, &session, DESTINATION);
    assert_eq!(drain::dispatches(&fixture), 3);
    assert_eq!(child_dispatches(&fixture), 2);
    stop(&mut restarted);
}

fn submitting_child(fixture: &Fixture, environment: &[(&str, String)]) -> (Host, String, PathBuf) {
    // Enroll caller-topology tools before binding, including the child's submit_result.
    std::fs::write(
        fixture.root.join("grandchild-script.json"),
        "{\"version\":1,\"steps\":[]}",
    )
    .unwrap();
    busy_child_with(fixture, environment)
}

fn finish_child_script(fixture: &Fixture) -> PathBuf {
    let release = fixture.root.join("continued-child-result");
    // Provider rounds restart per turn; the resumed root must not script another spawn.
    let mut parent = spawning_script();
    parent["product_child_script"] = json!(fixture.root.join("live-child-script.json"));
    parent["steps"] = json!([{"output":{"text":ANSWER}}]);
    fixture.write_script(&parent);
    std::fs::write(
        fixture.root.join("live-child-script.json"),
        serde_json::to_vec(&json!({"version":1,"repeat_last_step":true,"steps":[{
            "stream":[{"gate":{"file":release}}],
            "output":{"tool":{"name":"submit_result","arguments":{
                "turn_token":1,"output_json":"{\"summary\":\"continued child finished\"}"
            }}},"finish":"tool_calls"
        }]}))
        .unwrap(),
    )
    .unwrap();
    release
}

pub(super) fn root_continued(fixture: &Fixture, session: &str, key: &str) {
    let database = rusqlite::Connection::open_with_flags(
        fixture
            .home()
            .join("state")
            .join(key)
            .join("product.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        let state: String = database
            .query_row(
                "SELECT state FROM session_views WHERE session_id=?1",
                [session],
                |row| row.get(0),
            )
            .unwrap();
        let state: plakous_hosted::sessions::projector::SessionViewState =
            serde_json::from_str(&state).unwrap();
        let open: i64 = database
            .query_row(
                "SELECT count(*) FROM session_open_turn_views WHERE session_id=?1",
                [session],
                |row| row.get(0),
            )
            .unwrap();
        if state.counters.root_turns == 2 && open == 0 {
            let decisions:i64=database.query_row("SELECT count(*) FROM host_intents WHERE session_id=?1 AND kind='continuation_decision'",[session],|row|row.get(0)).unwrap();
            assert_eq!(
                decisions, 1,
                "a handoff allocated another continuation decision"
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "root continuation did not settle: {state:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn child_dispatches(fixture: &Fixture) -> usize {
    plakous_offline_crash_harness::evidence::read_evidence(
        fixture.root.join("child-evidence.jsonl"),
    )
    .unwrap()
    .iter()
    .filter(|row| {
        row.kind == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched
    })
    .count()
}
