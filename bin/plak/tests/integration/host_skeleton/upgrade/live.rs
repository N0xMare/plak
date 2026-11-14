//! Source live cuts reuse inspection and the joined step drain before surrender.
use super::*;
use plak::store::session_records::SessionRecordStore as _;

const ENABLED: &str = "PLAK_TEST_HOST_UPGRADE_LIVE";

mod retained;

fn busy_child(fixture: &Fixture) -> (Host, String, PathBuf) {
    let child_script = fixture.root.join("live-child-script.json");
    let release = fixture.root.join("live-child-release");
    write_private(
        &child_script,
        &json!({"version":1,"steps":[{
            "stream":[{"gate":{"file":release}}],"output":{"text":"child finished"}
        }]})
        .to_string(),
    );
    let mut script = spawning_script();
    script["product_child_script"] = json!(child_script);
    script["steps"][1]["output"]["tool"]["name"] = json!("wait");
    script["steps"][1]["output"]["tool"]["arguments"] = json!({"timeout_ms":300_000});
    fixture.write_script(&script);
    let host = start_env(
        fixture,
        &[],
        &[
            (ENABLED, "1".into()),
            ("PLAK_TEST_HOST_UPGRADE_CUT", "move-after-commit".into()),
        ],
    );
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created")
    };
    let attachment = client.attach(2, &node);
    client.retain_observations();
    client.read_deadline = Some(Instant::now() + WAIT);
    assert_eq!(
        client.request(
            3,
            Op::Intent {
                client_id: Some(uuid::Uuid::new_v4().to_string().parse().unwrap()),
                attachment,
                intent: SessionIntent::Prompt(Draft::from("wait for the delegated assignment")),
                blobs: Vec::new(),
            }
        ),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    drain::observations::tool_row(&mut client, "wait", false);
    let deadline = Instant::now() + WAIT;
    while plakous_offline_crash_harness::evidence::read_evidence(
        fixture.root.join("child-evidence.jsonl"),
    )
    .unwrap_or_default()
    .iter()
    .all(|row| {
        row.kind != plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched
    }) {
        assert!(
            Instant::now() < deadline,
            "the child never crossed provider dispatch"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    (host, session, release)
}

#[test]
fn host_move_live_joins_a_running_child_without_replacing_its_attempt_or_line() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let (mut host, session, release) = busy_child(&fixture);
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let record = plak::store::session_records::SqliteSessionStore::new(paths.clone())
        .load(&session)
        .unwrap();
    let (tree, _) = plakous_recursive_subagents_store::SqliteTreeStore::inspect_move_tree(
        &paths.subagents_database(),
        &record.node.tree_id,
    )
    .unwrap()
    .unwrap();
    let child = tree
        .rows()
        .nodes()
        .values()
        .find(|node| node.parent_agent_id.is_some())
        .unwrap()
        .clone();
    assert!(child.active_turn);
    let calls = drain::dispatches(&fixture);
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
    let diagnostic = if result.status.success() {
        String::new()
    } else {
        tree_cut_diagnostic(&paths, &record.node.tree_id)
    };
    // Stop at source surrender, before a destination could legitimately resume
    // this child. The source cut, not destination scheduling, is the oracle.
    let authority = if fixture.home().join("host/upgrades").exists() {
        receipts(&fixture)
    } else {
        Vec::new()
    };
    assert_eq!(
        authority
            .first()
            .and_then(|receipt| receipt["phase"].as_str()),
        Some("committed"),
        "{}; {diagnostic}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        !result.status.success(),
        "the source crash cut did not fire"
    );
    let (target, _) = plakous_recursive_subagents_store::SqliteTreeStore::inspect_move_tree(
        &paths.subagents_database(),
        &record.node.tree_id,
    )
    .unwrap()
    .unwrap();
    let after = &target.rows().nodes()[&child.agent_id];
    assert_eq!(after.attempt_id, child.attempt_id);
    assert_eq!(after.line_id, child.line_id);
    assert!(!after.active_turn);
    assert!(target.rows().loss_continuation(&child.agent_id).is_some());
    let before: plakous::sessions::SessionLocator = child.session_locator.unwrap().parse().unwrap();
    let after: plakous::sessions::SessionLocator =
        after.session_locator.as_ref().unwrap().parse().unwrap();
    assert_eq!(after.session_id(), before.session_id());
    assert_eq!(after.branch_id(), before.branch_id());
    assert_eq!(
        drain::dispatches(&fixture),
        calls,
        "copying a cut dispatched model work"
    );
    assert!(!release.exists());
    assert_eq!(host.child.take().unwrap().wait().unwrap().code(), Some(86));
}

fn tree_cut_diagnostic(paths: &plak::home::paths::PlakPaths, id: &str) -> String {
    let (tree, _) = plakous_recursive_subagents_store::SqliteTreeStore::inspect_move_tree(
        &paths.subagents_database(),
        id,
    )
    .unwrap()
    .unwrap();
    format!(
        "nodes={:?}; operations={:?}",
        tree.rows()
            .nodes()
            .values()
            .map(|node| (&node.agent_id, node.fold_order, node.active_turn))
            .collect::<Vec<_>>(),
        tree.rows()
            .operations()
            .values()
            .map(|operation| (
                &operation.tool_name,
                &operation.caller_agent_id,
                operation.fold,
                operation.terminal_code,
                operation.terminal_response.is_some()
            ))
            .collect::<Vec<_>>()
    )
}

#[test]
fn host_move_live_busy_root_reaches_a_joined_cut_before_guarded_surrender() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let (mut host, _client, session, _node, release) =
        drain::busy_with(&fixture, &[(ENABLED, "1".into())]);
    runner::wait_local_runner_ready(&host);
    let before = plak::store::session_records::SqliteSessionStore::new(
        plak::home::paths::PlakPaths::explicit(fixture.home()),
    )
    .load(&session)
    .unwrap();
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
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let authority = receipts(&fixture);
    assert_eq!(authority[0]["phase"], "acknowledged");
    assert_eq!(authority[0]["mappings"][0]["source"], session);
    assert_eq!(authority[0]["mappings"][0]["successor"], session);
    assert_eq!(authority[0]["groups"][0]["source_retired"], true);
    assert!(
        !release.exists(),
        "the source model was released instead of joined and interrupted"
    );
    let (after, _) = dormant::identity(&fixture, DESTINATION, &session);
    assert_eq!(
        after.session_id(),
        before.plakous_locator.unwrap().session_id()
    );
    assert_eq!(
        after.branch_id(),
        before.plakous_locator.unwrap().branch_id()
    );
    assert_eq!(
        drain::dispatches(&fixture),
        1,
        "moving metadata dispatched another model call"
    );
    stop(&mut host);
}

#[test]
fn host_move_live_binding_regression_refuses_before_drain_or_handoff() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let candidate = replacement_image(&fixture);
    let (mut host, mut client, session, _node, release) = drain::busy_with(
        &fixture,
        &[
            (ENABLED, "1".into()),
            (
                "PLAK_TEST_BINDING_CUT_IMAGE",
                candidate.display().to_string(),
            ),
        ],
    );
    runner::wait_local_runner_ready(&host);
    let probe = image_command(&fixture, &candidate)
        .args(["version", "--host-upgrade-probe"])
        .env("PLAK_TEST_STATE_KEY", DESTINATION)
        .output()
        .unwrap();
    let probe: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
    let result = image_command(&fixture, &candidate)
        .args([
            "host",
            "upgrade",
            "--expect-binary",
            probe["binary_id"].as_str().unwrap(),
            "--drain",
            "0",
        ])
        .env(ENABLED, "1")
        .env("PLAK_TEST_STATE_KEY", DESTINATION)
        .output()
        .unwrap();
    assert!(!result.status.success());
    let detail = String::from_utf8_lossy(&result.stderr);
    assert!(detail.contains("binding_preflight_refused"), "{detail}");
    assert!(!fixture.home().join("host/upgrades").exists());
    assert_eq!(client.request(40, Op::Ping), Outcome::Ok(Answer::Pong));
    let store = plak::store::session_records::SqliteSessionStore::new(
        plak::home::paths::PlakPaths::explicit(fixture.home()),
    );
    assert_eq!(store.load_intents(&session).unwrap().root_continuations, []);
    assert_eq!(drain::dispatches(&fixture), 1);
    std::fs::write(release, "finish the original admitted call").unwrap();
    stop(&mut host);
}
