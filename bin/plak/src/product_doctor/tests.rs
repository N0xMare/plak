use super::*;

#[test]
fn doctor_explains_colocated_git_selection_without_jj() {
    if crate::build_identity::JJ_ENABLED {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join(".jj")).unwrap();
    let check = cwd_check(Ok(directory.path().to_path_buf()));
    assert_eq!(check.severity, Severity::Error);
    assert!(check.remediation.unwrap().contains("jj feature"));
    fs::create_dir(directory.path().join(".git")).unwrap();
    let check = cwd_check(Ok(directory.path().to_path_buf()));
    assert_eq!(check.severity, Severity::Ok);
    assert!(check.detail.contains("using git copies"));
}

#[test]
fn doctor_reports_old_telemetry_files_without_modifying_them()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let current = directory.path().join("telemetry-v10.sqlite3");
    for name in ["telemetry.sqlite3", "telemetry-v9.sqlite3"] {
        fs::write(directory.path().join(name), b"retained")?;
    }
    fs::write(&current, b"current")?;
    let check = telemetry_check(
        CheckKind::TelemetryDatabase,
        &current,
        TelemetryHealth::Ok {
            rows: 1,
            write_errors: 0,
            dropped: 0,
        },
    );
    assert_eq!(check.severity, Severity::Warning);
    assert!(check.detail.contains("telemetry-v9.sqlite3"));
    assert!(check.detail.contains("telemetry.sqlite3"));
    assert!(!check.detail.contains("telemetry-v10.sqlite3"));
    assert_eq!(
        fs::read(directory.path().join("telemetry-v9.sqlite3"))?,
        b"retained"
    );
    Ok(())
}

#[test]
fn doctor_never_recommends_pruning_noncanonical_telemetry_names() {
    let directory = tempfile::tempdir().unwrap();
    let current = directory.path().join("telemetry-v20.sqlite3");
    for name in [
        "telemetry-v+5.sqlite3",
        "telemetry-v05.sqlite3",
        "telemetry-vbad.sqlite3",
        "telemetry-v20.sqlite3-wal",
    ] {
        fs::write(directory.path().join(name), b"retained").unwrap();
    }
    let check = telemetry_check(
        CheckKind::TelemetryDatabase,
        &current,
        TelemetryHealth::Ok {
            rows: 0,
            write_errors: 0,
            dropped: 0,
        },
    );
    assert_eq!(check.severity, Severity::Ok);
    assert_eq!(check.remediation, None);
    assert!(!check.detail.contains("telemetry-v"));
    fs::write(directory.path().join("telemetry-v5.sqlite3-wal"), b"orphan").unwrap();
    let check = telemetry_check(
        CheckKind::TelemetryDatabase,
        &current,
        TelemetryHealth::Missing,
    );
    assert_eq!(check.severity, Severity::Warning);
    assert!(check.detail.contains("telemetry-v5.sqlite3-wal"));
}

#[test]
fn doctor_preserves_current_telemetry_failure_remedy() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let current = directory.path().join("telemetry-v17.sqlite3");
    fs::write(directory.path().join("telemetry-v9.sqlite3"), b"retained")?;
    for reason in ["unavailable", "schema mismatch"] {
        let check = telemetry_check(
            CheckKind::TelemetryDatabase,
            &current,
            TelemetryHealth::Unavailable(reason.to_owned()),
        );
        assert_eq!(check.severity, Severity::Error);
        assert_eq!(
            check.remediation.as_deref(),
            Some("restore telemetry storage or explicitly remove the reported file to rebuild")
        );
        assert!(check.detail.starts_with(reason));
    }
    Ok(())
}

#[test]
fn doctor_labels_newer_telemetry_files_without_warning_or_removal_advice()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let current = directory.path().join("telemetry-v20.sqlite3");
    fs::write(directory.path().join("telemetry-v21.sqlite3"), b"retained")?;
    let check = telemetry_check(
        CheckKind::TelemetryDatabase,
        &current,
        TelemetryHealth::Ok {
            rows: 1,
            write_errors: 0,
            dropped: 0,
        },
    );
    assert_eq!(check.severity, Severity::Ok);
    assert_eq!(check.remediation, None);
    assert!(
        check
            .detail
            .contains("newer-schema files left untouched: telemetry-v21.sqlite3")
    );
    assert_eq!(
        fs::read(directory.path().join("telemetry-v21.sqlite3"))?,
        b"retained"
    );
    Ok(())
}

#[test]
fn store_pressure_is_full_at_the_ordinary_threshold_and_warns_one_reserve_before() {
    // A 16-checkpoint store: reserve 2, new work stops at 14.
    assert_eq!(StorePressure::of(11, 2, 14), StorePressure::Available);
    assert_eq!(StorePressure::of(12, 2, 14), StorePressure::NearlyFull);
    assert_eq!(StorePressure::of(14, 2, 14), StorePressure::Full);
    assert_eq!(StorePressure::of(17, 2, 14), StorePressure::Full);
    assert!(StorePressure::Full > StorePressure::NearlyFull);
}
#[test]
fn last_failed_refresh_fixture_ignores_began_and_clears_on_completed()
-> Result<(), Box<dyn std::error::Error>> {
    let (_guard, paths) = crate::product_paths::PlakPaths::isolated_test_home();
    paths.ensure_layout()?;
    let zone = jiff::tz::TimeZone::fixed(jiff::tz::offset(-4));
    assert_eq!(last_failed_refresh(&paths, "claude", &zone), None);
    assert!(!paths.telemetry_database().exists());
    drop(crate::telemetry_store::TelemetryStore::open(&paths)?);
    let db = rusqlite::Connection::open(paths.telemetry_database())?;
    // 2024-07-03 09:46:40 UTC, shown in a fixed UTC-4 zone.
    db.execute_batch(
        "INSERT INTO credential_refreshes
            (at, provider, phase, failure_class, transport_phase, generation) VALUES
            (1720000000000000000, 'claude', 'completed', NULL, NULL, 1),
            (1720000100000000000, 'claude', 'restored', 'timeout', 'resolution', 2),
            (1720000200000000000, 'claude', 'began', NULL, NULL, 3),
            (1720000300000000000, 'codex', 'completed', NULL, NULL, 9);",
    )?;
    assert_eq!(
        last_failed_refresh(&paths, "claude", &zone).as_deref(),
        Some(
            "refresh failed at 05:48 (generation 2): network unreachable (DNS lookup timed out); OAuth not recorded: not recorded; credential unchanged; retry when online"
        )
    );
    assert_eq!(last_failed_refresh(&paths, "codex", &zone), None);
    // Same timestamp is resolved by insertion id, never nondeterministically.
    db.execute_batch(
        "INSERT INTO credential_refreshes
            (at, provider, phase, generation) VALUES
            (1720000200000000000, 'claude', 'completed', 3),
            (1720000300000000000, 'claude', 'began', 4);",
    )?;
    assert_eq!(last_failed_refresh(&paths, "claude", &zone), None);
    db.execute_batch(
        "INSERT INTO credential_refreshes
            (at, provider, phase, failure_class, transport_phase, generation) VALUES
            (1720000200000000000, 'claude', 'outcome_unknown', 'transport', 'send', 4);",
    )?;
    let message = last_failed_refresh(&paths, "claude", &zone).ok_or("missing failure")?;
    assert!(message.contains("while sending"), "{message}");
    assert!(message.contains("refresh outcome unknown"), "{message}");
    assert!(!message.contains("credential unchanged"), "{message}");
    Ok(())
}

#[test]
fn last_failed_refresh_names_the_sub_reason_and_a_retired_record_needs_login()
-> Result<(), Box<dyn std::error::Error>> {
    let (_guard, paths) = crate::product_paths::PlakPaths::isolated_test_home();
    paths.ensure_layout()?;
    drop(crate::telemetry_store::TelemetryStore::open(&paths)?);
    let db = rusqlite::Connection::open(paths.telemetry_database())?;
    let zone = jiff::tz::TimeZone::UTC;
    db.execute_batch(
        "INSERT INTO credential_refreshes
            (at, provider, phase, failure_class, failure_reason, generation) VALUES
            (1720000100000000000, 'xai', 'restored', 'validation', 'id_token_exp', 2);",
    )?;
    let message = last_failed_refresh(&paths, "grok", &zone).ok_or("missing row")?;
    assert!(
        message.contains("refresh response could not be validated (id_token_exp)"),
        "{message}"
    );
    db.execute_batch(
        "INSERT INTO credential_refreshes
            (at, provider, phase, failure_class, failure_reason, generation) VALUES
            (1720000200000000000, 'codex', 'restored', 'validation', 'retired_format', 5);",
    )?;
    let message = last_failed_refresh(&paths, "codex", &zone).ok_or("missing row")?;
    assert!(
        message.contains("written by an older plak build"),
        "{message}"
    );
    assert!(!message.contains("could not be validated"), "{message}");
    assert!(message.ends_with("run plak login codex"), "{message}");
    assert!(!message.contains("plak refresh"), "{message}");
    Ok(())
}

#[test]
fn last_failed_refresh_hides_rows_older_than_the_current_credential()
-> Result<(), Box<dyn std::error::Error>> {
    assert!(refresh_row_is_current(215, None));
    assert!(refresh_row_is_current(215, Some(215)));
    assert!(refresh_row_is_current(215, Some(216)));
    assert!(!refresh_row_is_current(215, Some(217)));

    let (_guard, paths) = crate::product_paths::PlakPaths::isolated_test_home();
    paths.ensure_layout()?;
    let state = tempfile::tempdir()?;
    let record = state.path().join("plak/subscriptions/xai");
    fs::create_dir_all(&record)?;
    fs::write(
        record.join("credential.json"),
        br#"{"status":"ready","generation":217}"#,
    )?;
    install_credential_state_home(state.path().to_path_buf());
    drop(crate::telemetry_store::TelemetryStore::open(&paths)?);
    let db = rusqlite::Connection::open(paths.telemetry_database())?;
    db.execute_batch(
        "INSERT INTO credential_refreshes
            (at, provider, phase, failure_class, generation) VALUES
            (1720000100000000000, 'xai', 'restored', 'validation', 215);",
    )?;
    let zone = jiff::tz::TimeZone::UTC;
    assert_eq!(last_failed_refresh(&paths, "grok", &zone), None);
    db.execute_batch(
        "INSERT INTO credential_refreshes
            (at, provider, phase, failure_class, generation) VALUES
            (1720000200000000000, 'xai', 'restored', 'validation', 216);",
    )?;
    assert!(last_failed_refresh(&paths, "grok", &zone).is_some());
    Ok(())
}

#[test]
fn last_failed_refresh_fixture_non_network_and_quarantine_are_truthful()
-> Result<(), Box<dyn std::error::Error>> {
    let (_guard, paths) = crate::product_paths::PlakPaths::isolated_test_home();
    paths.ensure_layout()?;
    drop(crate::telemetry_store::TelemetryStore::open(&paths)?);
    let db = rusqlite::Connection::open(paths.telemetry_database())?;
    let zone = jiff::tz::TimeZone::UTC;
    for (class, phase, expected) in [
        ("http_4xx", "restored", "provider returned HTTP 401"),
        (
            "validation",
            "quarantined",
            "refresh response could not be validated",
        ),
        ("store", "outcome_unknown", "credential storage failure"),
        ("not_ready", "restored", "refresh dispatch was not ready"),
    ] {
        db.execute(
            "INSERT INTO credential_refreshes
                (at, provider, phase, failure_class, http_status, generation)
                VALUES (1720000100000000000, 'xai', ?1, ?2, 401, 2)",
            rusqlite::params![phase, class],
        )?;
        let message = last_failed_refresh(&paths, "grok", &zone).ok_or("missing row")?;
        assert!(message.contains(expected), "{message}");
        assert!(!message.contains("network unreachable"), "{message}");
        assert!(!message.contains("retry when online"), "{message}");
        assert_eq!(
            message.contains("credential unchanged"),
            phase == "restored"
        );
    }
    Ok(())
}

#[test]
fn last_failed_refresh_fixture_unreadable_schema_is_not_rebuilt()
-> Result<(), Box<dyn std::error::Error>> {
    let (_guard, paths) = crate::product_paths::PlakPaths::isolated_test_home();
    paths.ensure_layout()?;
    drop(crate::telemetry_store::TelemetryStore::open(&paths)?);
    let db = rusqlite::Connection::open(paths.telemetry_database())?;
    db.pragma_update(None, "user_version", 999)?;
    assert_eq!(
        last_failed_refresh(&paths, "claude", &jiff::tz::TimeZone::UTC),
        None
    );
    assert_eq!(
        db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?,
        999
    );
    Ok(())
}
