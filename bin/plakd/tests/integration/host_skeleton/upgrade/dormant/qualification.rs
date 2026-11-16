//! Actual process cuts around a lease-only, never-Ready Published abandonment.
use super::*;

const NATIVE_PUBLISHED: i64 = 3;
const NATIVE_ABANDONED: i64 = 4;

#[test]
fn host_move_published_abandon_before_native_commit_recovers_twice() {
    abandon_cut("move-before-abandon", NATIVE_PUBLISHED);
}

#[test]
fn host_move_published_abandon_after_native_commit_recovers_twice() {
    abandon_cut("move-after-native-abandon", NATIVE_ABANDONED);
}

#[test]
fn host_move_published_abandon_after_lease_only_reconciliation_opens_no_driver() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let (mut target, source) = published(&fixture, None);
    let receipt = receipts(&fixture).remove(0);
    let group = &receipt["groups"][0];
    assert_lease_only(&fixture, group);
    abandon(&fixture, group["id"].as_str().unwrap());
    assert_abandoned(&fixture, &source);
    stop(&mut target);
}

#[test]
fn host_move_published_abandon_refuses_after_an_ordinary_idle_driver_open_without_a_new_head() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let (mut target, source) = published_with_failure(&fixture, None, "reconcile-pause");
    let (head, _) = identity(&fixture, DESTINATION, &source);
    open_idle(&fixture, head);
    assert_eq!(identity(&fixture, DESTINATION, &source).0, head);
    let opened: i64 = target_native(&fixture)
        .query_row(
            "SELECT count(*) FROM imported_sessions WHERE execution_open=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(opened, 1);
    let receipt = receipts(&fixture).remove(0);
    let group_id = receipt["groups"][0]["id"].as_str().unwrap();
    for _ in 0..2 {
        let result = output_env(
            &fixture,
            &["host", "upgrade", "--abandon", group_id],
            true,
            &[(ENABLED, "1")],
        );
        assert!(
            !result.status.success(),
            "an ordinary idle open authorized abandonment"
        );
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("host_upgrade_group_abandon"),
            "{result:?}"
        );
        let receipt = receipts(&fixture).remove(0);
        assert_eq!(receipt["groups"][0]["phase"], "abandoning");
        assert!(receipt["groups"][0]["abandon_error"].is_string());
        assert_eq!(receipt["mappings"][0]["abandoned"], false);
        assert_eq!(native_phase(&fixture), NATIVE_PUBLISHED);
        assert_eq!(identity(&fixture, DESTINATION, &source).0, head);
    }
    assert!(!fixture.root.join("evidence.jsonl").exists() || crash::dispatches(&fixture) == 0);
    stop(&mut target);
}

fn open_idle(fixture: &Fixture, head: plakous::sessions::SessionLocator) {
    use plakous::sessions::{
        AgentSessions, OpenSessionOutcome, OpenSessionRequest, SessionBackend, SessionOperationId,
    };
    use plakous_offline_crash_harness::{BoundaryController, scripted_runtime_binding};
    let mut plan: plakous_offline_crash_harness::script::ScriptPlan =
        serde_json::from_slice(&std::fs::read(fixture.script()).unwrap()).unwrap();
    plan.profile_effort = Some(plakous::model::ReasoningEffort::Medium);
    let controller = BoundaryController::quiet_from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        fixture.root.join("idle-open-evidence.jsonl"),
    )
    .unwrap();
    let binding =
        scripted_runtime_binding(&fixture.workspace(), controller, std::sync::Arc::new(plan))
            .unwrap();
    let sessions = AgentSessions::new(
        SessionBackend::sqlite(
            fixture
                .home()
                .join("state")
                .join(DESTINATION)
                .join("runtime.sqlite3"),
        )
        .unwrap(),
    )
    .unwrap();
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    executor
        .block_on(async {
            let result = sessions
                .open(OpenSessionRequest::new(
                    SessionOperationId::new("qualification:ordinary-idle-open").unwrap(),
                    head,
                    binding,
                ))
                .await
                .unwrap();
            let OpenSessionOutcome::Opened { session, .. } = result else {
                panic!("ordinary idle open did not attach: {result:?}")
            };
            assert_eq!(session.locator(), head);
            assert_eq!(session.release().await.unwrap(), head);
        })
        .unwrap();
}

fn abandon_cut(cut: &str, expected_native: i64) {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let (mut target, source) = published(&fixture, Some(cut));
    let receipt = receipts(&fixture).remove(0);
    let group = &receipt["groups"][0];
    assert_lease_only(&fixture, group);
    let group_id = group["id"].as_str().unwrap();
    let result = output_env(
        &fixture,
        &["host", "upgrade", "--abandon", group_id],
        true,
        &[(ENABLED, "1")],
    );
    assert!(!result.status.success(), "cut was not reached: {cut}");
    assert_eq!(
        target.child.take().unwrap().wait().unwrap().code(),
        Some(86)
    );
    assert_eq!(receipts(&fixture)[0]["groups"][0]["phase"], "abandoning");
    assert_eq!(native_phase(&fixture), expected_native);
    for _ in 0..2 {
        let mut restored = start_env(
            &fixture,
            &[],
            &[
                (ENABLED, "1".into()),
                ("PLAK_TEST_STATE_KEY", DESTINATION.into()),
            ],
        );
        let deadline = Instant::now() + WAIT;
        while receipts(&fixture)[0]["groups"][0]["phase"] != "abandoned" {
            assert!(Instant::now() < deadline, "abandon recovery did not finish");
            std::thread::sleep(Duration::from_millis(20));
        }
        abandon(&fixture, group_id);
        assert_abandoned(&fixture, &source);
        stop(&mut restored);
    }
}

fn published(fixture: &Fixture, cut: Option<&str>) -> (Host, String) {
    published_with_failure(fixture, cut, "transfer-witness")
}

fn published_with_failure(fixture: &Fixture, cut: Option<&str>, failure: &str) -> (Host, String) {
    let source = seed(fixture, 91);
    let mut host = start_env(
        fixture,
        &[],
        &[
            (ENABLED, "1".into()),
            ("PLAK_TEST_HOST_UPGRADE_CUT", "move-after-publish".into()),
        ],
    );
    runner::wait_local_runner_ready(&host);
    let result = output_env(
        fixture,
        &["host", "upgrade", "--expect-binary", &binary(fixture)],
        true,
        &[(ENABLED, "1")],
    );
    assert!(!result.status.success());
    assert_eq!(host.child.take().unwrap().wait().unwrap().code(), Some(86));
    let mut environment = vec![
        (ENABLED, "1".into()),
        ("PLAK_TEST_STATE_KEY", DESTINATION.into()),
        ("PLAK_TEST_HOST_UPGRADE_FAILURE", failure.into()),
    ];
    if let Some(cut) = cut {
        environment.push(("PLAK_TEST_HOST_UPGRADE_CUT", cut.into()));
    }
    let target = start_env(fixture, &[], &environment);
    runner::wait_local_runner_ready(&target);
    let deadline = Instant::now() + WAIT;
    loop {
        let receipt = receipts(fixture).remove(0);
        let group = &receipt["groups"][0];
        assert_eq!(group["phase"], "published");
        if failure == "reconcile-pause" || !group["transfers"].as_object().unwrap().is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "lease-only reconciliation did not run"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    (target, source)
}

fn abandon(fixture: &Fixture, group: &str) {
    let result = output_env(
        fixture,
        &["host", "upgrade", "--abandon", group],
        true,
        &[(ENABLED, "1")],
    );
    assert!(result.status.success(), "{result:?}");
}

fn target_native(fixture: &Fixture) -> rusqlite::Connection {
    rusqlite::Connection::open_with_flags(
        fixture
            .home()
            .join("state")
            .join(DESTINATION)
            .join("runtime.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap()
}

fn native_phase(fixture: &Fixture) -> i64 {
    target_native(fixture)
        .query_row("SELECT phase FROM runtime_moves", [], |row| row.get(0))
        .unwrap()
}

fn assert_lease_only(fixture: &Fixture, group: &serde_json::Value) {
    assert_eq!(group["phase"], "published");
    assert!(!group["transfers"].as_object().unwrap().is_empty());
    let native = target_native(fixture);
    let opened: i64 = native
        .query_row(
            "SELECT count(*) FROM imported_sessions WHERE execution_open<>0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        opened, 0,
        "lease-only reconciliation opened an execution driver"
    );
    assert_eq!(native_phase(fixture), NATIVE_PUBLISHED);
}

fn assert_abandoned(fixture: &Fixture, source: &str) {
    let receipt = receipts(fixture).remove(0);
    assert_eq!(receipt["phase"], "acknowledged");
    assert_eq!(receipt["groups"][0]["phase"], "abandoned");
    assert_eq!(receipt["mappings"][0]["abandoned"], true);
    assert_eq!(receipt["mappings"][0]["ready"], false);
    assert_eq!(native_phase(fixture), NATIVE_ABANDONED);
    let result = output_env(
        fixture,
        &["run", "--session", source, "--", "must remain abandoned"],
        true,
        &[(ENABLED, "1")],
    );
    assert!(!result.status.success());
    assert!(!fixture.root.join("evidence.jsonl").exists() || crash::dispatches(fixture) == 0);
}
