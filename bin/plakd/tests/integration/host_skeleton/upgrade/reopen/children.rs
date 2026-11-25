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
    spawn_child_with_idle(fixture, host, |session| {
        root_wake::idle_turns(fixture, session, 1);
    })
}

pub(in crate::host_skeleton) fn spawn_child_with_idle(
    fixture: &Fixture,
    host: &Host,
    wait_idle: impl FnOnce(&str),
) -> (Client, String, NodeAddress) {
    let mut client = Client::connect(host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("root creation refused");
    };
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, "spawn a child", ANSWER);
    wait_idle(&session);
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
fn a_binding_fault_seen_by_both_images_is_not_a_regression() {
    let Some((fixture, _)) = root_wake::child_fixture(false) else {
        return;
    };
    let _cleanup = HomeHost(fixture.home());
    let image = replacement_image(&fixture);
    let both = std::env::join_paths([Path::new(env!("CARGO_BIN_EXE_plakd")), &image])
        .unwrap()
        .into_string()
        .unwrap();
    let mut host = start_env(
        &fixture,
        &[],
        &[("PLAK_TEST_CHILD_BINDING_CUT_IMAGE", both)],
    );
    let (_client, _, node) = spawn_child(&fixture, &host);
    std::fs::write(fixture.root.join("release-child"), "").unwrap();
    root_wake::submitted_result(&fixture, &node);
    let result = upgrade_to(&fixture, &image);
    require_upgrade(&fixture, &host, "pre-existing child binding", &result);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains(
            "host_upgrade_child_reopen_pre_existing: 1 affected child session(s); binding_change=1"
        ),
        "{stderr}"
    );
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

    std::fs::write(&evaluator, b"different evaluator bytes").unwrap();
    let changed = output(&fixture, &args, false);
    let verdict: serde_json::Value = serde_json::from_slice(&changed.stdout).unwrap();
    assert_eq!(verdict["verdict"], "opens", "{verdict}");
    let faults = &verdict["child_qualification"]["pre_existing"];
    assert_eq!(faults["affected_children"], 1, "{verdict}");
    assert_eq!(
        faults["by_kind"]["evaluator_artifact_bytes_mismatch"], 1,
        "{verdict}"
    );

    std::fs::remove_file(evaluator).unwrap();
    let mixed = output_env(
        &fixture,
        &args,
        false,
        &[(
            "PLAK_TEST_CHILD_BINDING_CUT_IMAGE",
            env!("CARGO_BIN_EXE_plakd"),
        )],
    );
    let verdict: serde_json::Value = serde_json::from_slice(&mixed.stdout).unwrap();
    assert_eq!(verdict["verdict"], "refused", "{verdict}");
    assert_eq!(
        verdict["children"]["affected_children"],
        CHILDREN - 2,
        "{verdict}"
    );
    let faults = &verdict["child_qualification"]["pre_existing"];
    assert_eq!(faults["affected_children"], 1, "{verdict}");
    assert_eq!(
        faults["by_kind"]["evaluator_artifact_missing"], 1,
        "{verdict}"
    );
}

#[test]
fn a_missing_evaluator_is_reported_and_does_not_refuse() {
    let Some((fixture, _)) = root_wake::child_fixture(false) else {
        return;
    };
    let image = replacement_image(&fixture);
    missing_evaluator_upgrade(
        &fixture,
        Path::new(env!("CARGO_BIN_EXE_plakd")),
        &image,
        |session| {
            root_wake::idle_turns(&fixture, session, 1);
        },
    );
}

pub(in crate::host_skeleton) fn missing_evaluator_upgrade(
    fixture: &Fixture,
    source: &Path,
    destination: &Path,
    wait_idle: impl FnOnce(&str),
) {
    let _cleanup = HomeHost(fixture.home());
    let start = || {
        await_serving_host(spawn_host(
            image_command(fixture, source)
                .env("PLAK_TEST_CHILD_BINDING_CUT_IMAGE", destination)
                .arg("host"),
        ))
        .unwrap()
    };
    let mut host = start();
    let (_client, _, node) = spawn_child_with_idle(fixture, &host, wait_idle);
    std::fs::write(fixture.root.join("release-child"), "").unwrap();
    root_wake::submitted_result(fixture, &node);
    stop(&mut host);

    let paths = PlakPaths::explicit(fixture.home());
    let evaluator = fixture.root.join("saved-evaluator");
    std::fs::write(&evaluator, b"recorded evaluator").unwrap();
    let locator = seed_evaluator_child(fixture, &paths, &evaluator);
    let db = rusqlite::Connection::open(paths.subagents_database()).unwrap();
    assert_eq!(
        db.execute(
            "UPDATE agent_nodes SET session_locator=?1, status='completed', active_turn=0
             WHERE tree_id=?2 AND parent_agent_id IS NOT NULL",
            rusqlite::params![locator, node.tree_id],
        )
        .unwrap(),
        1
    );
    drop(db);
    std::fs::remove_file(evaluator).unwrap();

    let mut host = start();
    let result = upgrade_to(fixture, destination);
    require_upgrade(fixture, &host, "pre-existing missing evaluator", &result);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("host_upgrade_child_reopen_pre_existing: 1 affected child session(s); evaluator_artifact_missing=1"),
        "{stderr}"
    );
    assert_eq!(
        stderr
            .matches("host_upgrade_child_reopen_pre_existing:")
            .count(),
        1,
        "{stderr}"
    );

    let audit = image_command(fixture, destination)
        .env("PLAK_TEST_CHILD_BINDING_CUT_IMAGE", destination)
        .args(["session", "check-binding", "--all"])
        .output()
        .unwrap();
    let audit: serde_json::Value = serde_json::from_slice(&audit.stdout).unwrap();
    let child = audit["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["child"] == true)
        .unwrap();
    assert!(
        child["child_binding"]["binding_change"].is_string(),
        "{child}"
    );
    assert!(child["binding_change"].is_null(), "{child}");

    stop(&mut host);
}

fn seed_evaluator_child(fixture: &Fixture, paths: &PlakPaths, evaluator: &Path) -> String {
    use plakous::sessions::rehearsal::{bootstrap_stopped_runtime, runtime_bootstrap_pending};

    let runtime = paths.runtime_database();
    let legacy_epoch = runtime_bootstrap_pending(&runtime).unwrap().then(|| {
        let epoch = rusqlite::Connection::open_with_flags(
            &runtime,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap()
        .query_row("SELECT root_codec FROM checkpoints LIMIT 1", [], |row| {
            row.get(0)
        })
        .unwrap();
        bootstrap_stopped_runtime(&runtime, |_| {}).unwrap();
        epoch
    });
    let locator = seed_child_branches(fixture, paths, 1, evaluator)
        .pop()
        .unwrap();
    // The fixture writer needs the new DDL; the pinned source needs its old
    // DDL. The authored checkpoint codec stays the same.
    if let Some(epoch) = legacy_epoch {
        runtime_epoch::make_pre_bootstrap(&runtime, epoch);
    }
    locator
}

fn upgrade_to(fixture: &Fixture, image: &Path) -> std::process::Output {
    let identity = image_command(fixture, image)
        .args(["version", "--host-upgrade-probe"])
        .output()
        .unwrap();
    let identity: serde_json::Value = serde_json::from_slice(&identity.stdout).unwrap();
    image_command(fixture, image)
        .args([
            "host",
            "upgrade",
            "--drain",
            "0",
            "--expect-binary",
            identity["binary_id"].as_str().unwrap(),
        ])
        .output()
        .unwrap()
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
                let binding = if index == (count.min(3) - 1) {
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
                if count > 1 && index == 1 {
                    sessions.archive(locator).await.unwrap();
                }
                locators.push(locator.to_string());
            }
            locators
        })
        .unwrap()
}
