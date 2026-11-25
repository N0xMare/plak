//! Child binding qualification beside a live owner and across a wide tree.

use super::*;
use plakous::sessions::{
    CreateSessionOutcome, CreateSessionRequest, SessionBackend, SessionOperationId,
};
use plakous_offline_crash_harness::{
    BoundaryController, protocol::CrashBoundary, script::ScriptPlan,
    scripted_code_mode_runtime_binding, scripted_runtime_binding,
};
use std::sync::{Arc, atomic::AtomicU64};

fn spawn_child(fixture: &Fixture, host: &Host) -> (Client, String, NodeAddress) {
    let mut client = Client::connect(host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("root creation refused");
    };
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, "spawn a child", ANSWER);
    root_wake::idle_turns(fixture, &session, 1);
    let paths = PlakPaths::explicit(fixture.home());
    let deadline = Instant::now() + WAIT;
    loop {
        let children = plakous_recursive_subagents_store::SqliteTreeStore::inspect_reopen_children(
            &paths.subagents_database(),
            &node.tree_id,
        )
        .unwrap();
        if !children.is_empty() {
            return (client, session, node);
        }
        assert!(Instant::now() < deadline, "child did not bind");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn child_binding_change_refuses_before_drain() {
    let Some((fixture, _)) = root_wake::child_fixture(false) else {
        return;
    };
    let _cleanup = HomeHost(fixture.home());
    let image = replacement_image(&fixture);
    let cut = image.to_str().unwrap();
    let mut host = start_env(
        &fixture,
        &[],
        &[("PLAK_TEST_CHILD_BINDING_CUT_IMAGE", cut.into())],
    );
    let (mut client, session, node) = spawn_child(&fixture, &host);

    let probe = image_command(&fixture, &image)
        .env("PLAK_TEST_CHILD_BINDING_CUT_IMAGE", cut)
        .args(["version", "--reopen-probe", &session])
        .output()
        .unwrap();
    assert!(probe.status.success(), "{probe:?}");
    let verdict: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
    assert_eq!(verdict["verdict"], "refused", "{verdict}");
    assert_eq!(verdict["children"]["affected_children"], 1, "{verdict}");
    assert_eq!(
        verdict["children"]["binding_change"],
        "executor binding for child tool"
    );

    let identity = image_command(&fixture, &image)
        .args(["version", "--host-upgrade-probe"])
        .output()
        .unwrap();
    let identity: serde_json::Value = serde_json::from_slice(&identity.stdout).unwrap();
    let result = image_command(&fixture, &image)
        .args([
            "host",
            "upgrade",
            "--drain",
            "0",
            "--expect-binary",
            identity["binary_id"].as_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("host_upgrade_child_reopen_refused: 1 affected child session(s)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("executor binding for child tool"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("resolving earlier admission verdicts"),
        "{stderr}"
    );
    assert!(matches!(
        client.request(4, new_session(&fixture.workspace())),
        Outcome::Ok(Answer::Created { .. })
    ));
    std::fs::write(fixture.root.join("release-child"), "").unwrap();
    root_wake::submitted_result(&fixture, &node);
    stop(&mut host);
}

#[test]
fn a_wide_dormant_tree_qualifies_with_one_bounded_probe() {
    const CHILDREN: usize = 96;
    let Some((fixture, _)) = root_wake::child_fixture(false) else {
        return;
    };
    let _cleanup = HomeHost(fixture.home());
    let mut host = start(&fixture);
    let (_client, session, node) = spawn_child(&fixture, &host);
    std::fs::write(fixture.root.join("release-child"), "").unwrap();
    root_wake::submitted_result(&fixture, &node);
    stop(&mut host);

    let paths = PlakPaths::explicit(fixture.home());
    let evaluator = fixture.root.join("saved-evaluator");
    std::fs::write(&evaluator, b"original evaluator bytes").unwrap();
    let locators = seed_child_branches(&fixture, &paths, CHILDREN, &evaluator);
    let database = rusqlite::Connection::open(paths.subagents_database()).unwrap();
    let tx = database.unchecked_transaction().unwrap();
    tx.execute("CREATE TEMP TABLE prototype AS SELECT * FROM agent_nodes WHERE tree_id=?1 AND parent_agent_id IS NOT NULL LIMIT 1", [&node.tree_id]).unwrap();
    for (index, locator) in locators.iter().enumerate() {
        tx.execute(
            "UPDATE prototype SET agent_id=?1,session_locator=?2,attempt_id=NULL,
             status=?3,active_turn=0",
            rusqlite::params![
                format!("c{}", 1000 + index),
                locator,
                if index == 0 { "closed" } else { "completed" }
            ],
        )
        .unwrap();
        assert_eq!(
            tx.execute("INSERT INTO agent_nodes SELECT * FROM prototype", [])
                .unwrap(),
            1
        );
    }
    tx.commit().unwrap();
    let args = [
        "version",
        "--host-upgrade-reopen",
        "--reopen-probe",
        &session,
    ];
    let began = Instant::now();
    let opened = output(&fixture, &args, false);
    assert!(opened.status.success(), "{opened:?}");
    let verdict: serde_json::Value = serde_json::from_slice(&opened.stdout).unwrap();
    assert_eq!(verdict["verdict"], "opens", "{verdict}");
    assert!(
        began.elapsed() < Duration::from_secs(15),
        "wide-tree probe took {:?}",
        began.elapsed()
    );
    let changed = output_env(
        &fixture,
        &args,
        false,
        &[(
            "PLAK_TEST_CHILD_BINDING_CUT_IMAGE",
            env!("CARGO_BIN_EXE_plakd"),
        )],
    );
    assert!(changed.status.success(), "{changed:?}");
    let verdict: serde_json::Value = serde_json::from_slice(&changed.stdout).unwrap();
    // The original live fixture child remained; one seeded branch is closed
    // and another archived, leaving CHILDREN - 1 affected branches.
    assert_eq!(
        verdict["children"]["affected_children"],
        CHILDREN - 1,
        "{verdict}"
    );

    // No test cut is active: changing the bytes of the one recorded evaluator
    // must refuse that branch through the same check as a real exact reopen.
    std::fs::write(evaluator, b"different evaluator bytes").unwrap();
    let changed = output(&fixture, &args, false);
    let verdict: serde_json::Value = serde_json::from_slice(&changed.stdout).unwrap();
    assert_eq!(verdict["children"]["affected_children"], 1, "{verdict}");
    assert!(
        verdict["children"]["binding_change"]
            .as_str()
            .unwrap()
            .contains("recorded evaluator"),
        "{verdict}"
    );
}

fn seed_child_branches(
    fixture: &Fixture,
    paths: &PlakPaths,
    count: usize,
    evaluator: &Path,
) -> Vec<String> {
    let backend = SessionBackend::sqlite(paths.runtime_database()).unwrap();
    let sessions = AgentSessions::new(backend).unwrap();
    let runner = plakous::application::ApplicationExecutor::new().unwrap();
    let script = Arc::new(ScriptPlan::read(&fixture.script()).unwrap());
    let evaluated_script_path = fixture.root.join("evaluated-script.json");
    std::fs::write(
        &evaluated_script_path,
        json!({"version":1, "code_mode":true, "js_ptc":true, "steps":[]}).to_string(),
    )
    .unwrap();
    let evaluated_script = Arc::new(ScriptPlan::read(&evaluated_script_path).unwrap());
    let control = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        fixture.root.join("wide-evidence"),
    )
    .unwrap();
    runner
        .block_on(async {
            let mut locators = Vec::with_capacity(count);
            for index in 0..count {
                let binding = if index == 2 {
                    scripted_code_mode_runtime_binding(
                        &fixture.workspace(),
                        control.clone(),
                        Arc::clone(&evaluated_script),
                        Arc::new(AtomicU64::new(1)),
                        evaluator,
                        None,
                    )
                } else {
                    scripted_runtime_binding(
                        &fixture.workspace(),
                        control.clone(),
                        Arc::clone(&script),
                    )
                }
                .unwrap();
                let CreateSessionOutcome::Created { session, .. } = sessions
                    .create(CreateSessionRequest::new(
                        SessionOperationId::new(format!("wide-child-{index}")).unwrap(),
                        binding,
                    ))
                    .await
                    .unwrap()
                else {
                    panic!("child session not created");
                };
                let locator = session.release().await.unwrap();
                if index == 1 {
                    sessions.archive(locator).await.unwrap();
                }
                locators.push(locator.to_string());
            }
            locators
        })
        .unwrap()
}
