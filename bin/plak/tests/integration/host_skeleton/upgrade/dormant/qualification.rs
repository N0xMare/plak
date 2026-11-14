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
        ("PLAK_TEST_HOST_UPGRADE_FAILURE", "transfer-witness".into()),
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
        if !group["transfers"].as_object().unwrap().is_empty() {
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
