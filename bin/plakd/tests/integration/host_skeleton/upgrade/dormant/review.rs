use super::*;

#[test]
fn host_move_aborted_foreign_start_with_missing_producer_withdraws_native_freeze_twice() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let source = seed(&fixture, 81);
    let mut host = start_env(
        &fixture,
        &[],
        &[
            (ENABLED, "1".into()),
            (
                "PLAK_TEST_HOST_UPGRADE_CUT",
                "move-after-source-prepare".into(),
            ),
        ],
    );
    runner::wait_local_runner_ready(&host);
    let result = output_env(
        &fixture,
        &["host", "upgrade", "--expect-binary", &binary(&fixture)],
        true,
        &[(ENABLED, "1")],
    );
    assert!(!result.status.success());
    assert_eq!(host.child.take().unwrap().wait().unwrap().code(), Some(86));
    let mut authority = receipts(&fixture).remove(0);
    assert_eq!(authority["source_archival_pending"], false);
    authority["source_executable"] = json!(fixture.root.join("missing-producer"));
    let path = fixture
        .home()
        .join("host/upgrades")
        .join(authority["id"].as_str().unwrap())
        .join("receipt.json");
    write_private(&path, &serde_json::to_string(&authority).unwrap());
    let mut foreign = start_env(
        &fixture,
        &[],
        &[
            (ENABLED, "1".into()),
            ("PLAK_TEST_STATE_KEY", DESTINATION.into()),
        ],
    );
    assert_eq!(receipts(&fixture)[0]["phase"], "aborted");
    stop(&mut foreign);
    for pass in 0..2 {
        let mut restored = start_env(&fixture, &[], &[(ENABLED, "1".into())]);
        assert_eq!(
            export(&fixture, &source, false, &format!("withdrawn-{pass}.json"))["queue"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        stop(&mut restored);
    }
}

#[test]
fn host_move_committed_finish_failure_is_deferred_without_reopening_source() {
    decision_failure("source-finish", "acknowledged");
}

#[test]
fn host_move_uncertain_decision_restarts_without_resuming_source_admissions() {
    decision_failure("commit-before-save", "aborted");
    decision_failure("commit-after-save", "acknowledged");
}

#[test]
fn host_move_post_prepare_membership_divergence_refuses_and_withdraws_while_prepared() {
    for failure in ["reservation-lines", "reservation-runners"] {
        let Some(fixture) = fixture() else { return };
        let _cleanup = HomeHost(fixture.home());
        let source = seed(&fixture, 84);
        let mut host = start_env(
            &fixture,
            &[],
            &[
                (ENABLED, "1".into()),
                ("PLAK_TEST_HOST_UPGRADE_FAILURE", failure.into()),
            ],
        );
        runner::wait_local_runner_ready(&host);
        let result = output_env(
            &fixture,
            &["host", "upgrade", "--expect-binary", &binary(&fixture)],
            true,
            &[(ENABLED, "1")],
        );
        assert!(!result.status.success());
        assert!(
            String::from_utf8_lossy(&result.stderr)
                .contains("prepared group differs from its durable reservation"),
            "{result:?}"
        );
        let receipt = receipts(&fixture).remove(0);
        assert_eq!(receipt["phase"], "aborted");
        assert_eq!(receipt["source_archival_pending"], false);
        assert_eq!(receipt["groups"][0]["source_retired"], false);
        let capsule = fixture
            .home()
            .join("host/upgrades")
            .join(receipt["id"].as_str().unwrap())
            .join(format!(
                "{}.json",
                receipt["groups"][0]["id"].as_str().unwrap()
            ));
        assert!(
            !capsule.exists(),
            "reservation mismatch reached group export"
        );
        assert_eq!(
            export(&fixture, &source, false, "withdrawn-membership.json")["queue"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(matches!(
            Client::connect(&host).request(1, new_session(&fixture.workspace())),
            Outcome::Ok(Answer::Created { .. })
        ));
        stop(&mut host);
    }
}

fn decision_failure(failure: &str, phase: &str) {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let source = seed(&fixture, 82);
    let host = start_env(
        &fixture,
        &[],
        &[
            (ENABLED, "1".into()),
            ("PLAK_TEST_HOST_UPGRADE_FAILURE", failure.into()),
        ],
    );
    runner::wait_local_runner_ready(&host);
    let result = output_env(
        &fixture,
        &["host", "upgrade", "--expect-binary", &binary(&fixture)],
        true,
        &[(ENABLED, "1")],
    );
    if failure == "commit-before-save" {
        assert!(!result.status.success());
        assert!(
            String::from_utf8_lossy(&result.stderr)
                .contains("decision pending startup reconciliation; check `plakd status`"),
            "{result:?}"
        );
    }
    let deadline = Instant::now() + WAIT;
    while receipts(&fixture)[0]["phase"] != phase && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let receipt = receipts(&fixture).remove(0);
    assert_eq!(
        receipt["phase"],
        phase,
        "{failure}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    // Replacement uses exec: the PID can remain live, but the old serving
    // instance must end rather than treating the fault as a precommit refusal.
    loop {
        let line = host
            .lines
            .recv_timeout(WAIT)
            .expect("replacement must serve after receipt reconciliation");
        if line.starts_with("plakd host: serving on ") {
            assert!(!line.contains(receipt["source_instance"].as_str().unwrap()));
            break;
        }
    }
    if phase == "acknowledged" {
        assert_retired(&fixture, &receipt["groups"][0]);
        assert_eq!(successor(&fixture, &source), source);
    } else {
        assert_eq!(
            export(&fixture, &source, false, "owed-after-uncertain.json")["queue"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}

#[test]
fn host_move_exact_transfer_refusal_and_final_sweep_withhold_published_group() {
    withheld("transfer-witness");
    withheld("post-transfer-witness");
}

fn withheld(failure: &str) {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let source = seed(&fixture, 83);
    let mut host = start_env(
        &fixture,
        &[],
        &[
            (ENABLED, "1".into()),
            ("PLAK_TEST_HOST_UPGRADE_CUT", "move-after-publish".into()),
        ],
    );
    runner::wait_local_runner_ready(&host);
    let result = output_env(
        &fixture,
        &["host", "upgrade", "--expect-binary", &binary(&fixture)],
        true,
        &[(ENABLED, "1")],
    );
    assert!(!result.status.success());
    host.child.take().unwrap().wait().unwrap();
    let mut target = start_env(
        &fixture,
        &[],
        &[
            (ENABLED, "1".into()),
            ("PLAK_TEST_STATE_KEY", DESTINATION.into()),
            ("PLAK_TEST_HOST_UPGRADE_FAILURE", failure.into()),
        ],
    );
    runner::wait_local_runner_ready(&target);
    let deadline = Instant::now() + WAIT;
    while receipts(&fixture)[0]["groups"][0]["transfers"]
        .as_object()
        .unwrap()
        .is_empty()
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(300));
    let receipt = receipts(&fixture).remove(0);
    assert_eq!(receipt["groups"][0]["phase"], "published");
    assert!(
        !receipt["groups"][0]["transfers"]
            .as_object()
            .unwrap()
            .is_empty()
    );
    let group = receipt["groups"][0]["id"].as_str().unwrap();
    let abandoned = output_env(
        &fixture,
        &["host", "upgrade", "--abandon", group],
        true,
        &[(ENABLED, "1")],
    );
    assert!(
        abandoned.status.success(),
        "{}",
        String::from_utf8_lossy(&abandoned.stderr)
    );
    assert_eq!(receipts(&fixture)[0]["groups"][0]["phase"], "abandoned");
    let refused = output_env(
        &fixture,
        &["run", "--session", &source, "--", "never"],
        true,
        &[(ENABLED, "1")],
    );
    assert!(!refused.status.success());
    stop(&mut target);
}

pub(super) fn assert_retired(fixture: &Fixture, group: &serde_json::Value) {
    let db = rusqlite::Connection::open_with_flags(
        fixture
            .home()
            .join("state")
            .join(plakd::home::state::state_key())
            .join("runtime.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let operation = group["operation"]
        .as_array()
        .unwrap()
        .iter()
        .map(|byte| u8::try_from(byte.as_u64().unwrap()).unwrap())
        .collect::<Vec<_>>();
    let phase: i64 = db
        .query_row(
            "SELECT phase FROM runtime_moves WHERE operation=?1",
            [operation],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(phase, 1, "receipt retirement must follow the native Commit");
    let sessions = plakous::sessions::AgentSessions::new(
        plakous::sessions::SessionBackend::sqlite(
            fixture
                .home()
                .join("state")
                .join(plakd::home::state::state_key())
                .join("runtime.sqlite3"),
        )
        .unwrap(),
    )
    .unwrap();
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    for line in group["lines"].as_array().unwrap() {
        let line = line.as_str().unwrap().parse().unwrap();
        assert!(
            executor
                .block_on(sessions.line_head(line))
                .unwrap()
                .is_err()
        );
        let operation = plakous::sessions::SessionOperationId::new("retired-line-probe").unwrap();
        assert!(
            executor
                .block_on(sessions.open_lease_only(&operation, line))
                .unwrap()
                .is_err()
        );
    }
}
