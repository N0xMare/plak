use super::*;
use std::path::Path;

const SEAL: &str = "SELECT hex(validation_seal) FROM checkpoints WHERE root_codec=38";

#[test]
fn paired_handoff_preserves_previous_epoch_records() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let runtime = paths.runtime_database();
    // Publishing the host socket does not guarantee its startup scan has
    // already opened an empty runtime store.
    std::fs::create_dir_all(runtime.parent().unwrap()).unwrap();
    drop(plakous::sessions::SessionBackend::sqlite(&runtime).unwrap());
    let mut source = start(&fixture);
    let image = replacement_image(&fixture).canonicalize().unwrap();
    let legacy = fixture.root.join("previous.sqlite3");
    copy_legacy_fixture(&legacy);
    seed_legacy(&runtime, &legacy);
    let seal = query(&legacy, SEAL);
    let command = || image_command(&fixture, &image);
    let probe = command()
        .args(["version", "--host-upgrade-probe"])
        .output()
        .unwrap();
    assert!(probe.status.success(), "{probe:?}");
    let verdict: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
    let upgraded = command()
        .args([
            "host",
            "upgrade",
            "--expect-binary",
            verdict["binary_id"].as_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(upgraded.status.success(), "{upgraded:?}");
    assert!(
        String::from_utf8_lossy(&upgraded.stderr).contains("bootstrap qualification completed")
    );
    assert_eq!(table_count(&runtime, "runtime_writer"), "1");
    assert_eq!(query(&runtime, SEAL), seal);
    assert_eq!(
        query(&runtime, "SELECT CAST(count(*) AS TEXT) FROM session_feed"),
        "3"
    );
    assert_eq!(
        Client::connect_at(&source.socket).request(1, Op::Ping),
        Outcome::Ok(Answer::Pong)
    );
    stop(&mut source);
}

#[test]
fn stopped_runtime_crash_before_commit_resumes_bootstrap() {
    stopped_runtime_crash("runtime:before-commit", false);
}

#[test]
fn stopped_runtime_crash_after_commit_reopens_before_first_write() {
    stopped_runtime_crash("runtime:after-commit", true);
}

fn stopped_runtime_crash(cut: &str, committed: bool) {
    use std::os::unix::fs::PermissionsExt as _;

    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    stop(&mut start(&fixture));
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let runtime = paths.runtime_database();
    copy_legacy_fixture(&runtime);
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o600)).unwrap();
    let seal = query(&runtime, SEAL);

    let environment = [("PLAK_TEST_HOST_UPGRADE_CUT", cut.to_owned())];
    let (child, lines) = spawn_env(&fixture, &[], &environment);
    let (code, stderr) = revision_cuts::exit(child, &lines);
    assert_eq!(code, Some(86), "{cut}: {stderr}");
    let backup = runtime.with_extension("pre-codec-bootstrap.sqlite3");
    assert_eq!(query(&backup, SEAL), seal);
    assert_eq!(table_count(&backup, "runtime_writer"), "0");
    assert_eq!(
        table_count(&runtime, "runtime_writer"),
        if committed { "1" } else { "0" }
    );
    assert_eq!(query(&runtime, SEAL), seal);

    let modified = backup.metadata().unwrap().modified().unwrap();
    let mut resumed = start_env(&fixture, &[], if committed { &environment } else { &[] });
    assert_eq!(
        Client::connect(&resumed).request(1, Op::Ping),
        Outcome::Ok(Answer::Pong)
    );
    assert_eq!(table_count(&runtime, "runtime_writer"), "1");
    assert_eq!(query(&runtime, SEAL), seal);
    if committed {
        assert_eq!(backup.metadata().unwrap().modified().unwrap(), modified);
    }
    stop(&mut resumed);
}

fn copy_legacy_fixture(path: &Path) {
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/store/tests/fixtures/epoch-38/runtime.sqlite3"),
        path,
    )
    .unwrap();
}

fn seed_legacy(runtime: &Path, legacy: &Path) {
    make_pre_bootstrap(runtime, 38);
    let mut connection = rusqlite::Connection::open(runtime).unwrap();
    connection.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    connection
        .execute("ATTACH DATABASE ?1 AS prior", [legacy.to_str().unwrap()])
        .unwrap();
    let tx = connection.transaction().unwrap();
    for name in [
        "runtime_objects",
        "runtime_object_bytes",
        "runtime_object_edges",
        "checkpoints",
        "sessions",
        "branches",
        "create_receipts",
        "session_feed",
        "lines",
    ] {
        tx.execute_batch(&format!(
            "INSERT INTO main.{name} SELECT * FROM prior.{name}"
        ))
        .unwrap();
    }
    tx.execute_batch(
        "UPDATE main.store_meta SET (sessions, branches, checkpoints, objects, object_bytes,
         semantic_mutations, tombstones, reclaimed_objects, reclaimed_object_bytes) =
         (SELECT sessions, branches, checkpoints, objects, object_bytes, semantic_mutations,
          tombstones, reclaimed_objects, reclaimed_object_bytes FROM prior.store_meta)",
    )
    .unwrap();
    tx.commit().unwrap();
}

#[test]
fn runtime_bootstrap_rehearses_before_drain_and_keeps_the_state_key() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let session = seed(&fixture, 44);
    let image = replacement_image(&fixture).canonicalize().unwrap();
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let runtime = paths.runtime_database();
    let baseline = query(
        &runtime,
        "SELECT hex(validation_seal) FROM checkpoints LIMIT 1",
    );
    let mut source = start_env(&fixture, &[], &[]);
    let command = || image_command(&fixture, &image);
    let probe = command()
        .args(["version", "--host-upgrade-probe"])
        .output()
        .unwrap();
    assert!(probe.status.success(), "{probe:?}");
    let verdict: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
    let id = verdict["binary_id"].as_str().unwrap();
    let key = verdict["state_key"].as_str().unwrap();
    assert_eq!(key, plakd::home::state::state_key());
    let upgrade = || {
        command()
            .args(["host", "upgrade", "--expect-binary", id])
            .output()
            .unwrap()
    };

    // The incumbent is idle. Freeze storage at the pre-bootstrap shape as an
    // older writer would leave it. The unsupported case must not pause it.
    make_pre_bootstrap(&runtime, 39);
    let wrong = rusqlite::Connection::open(&runtime).unwrap();
    wrong
        .execute_batch("CREATE TABLE unexpected_runtime_shape (id INTEGER PRIMARY KEY)")
        .unwrap();
    drop(wrong);
    let refused = upgrade();
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("integrity"),
        "{refused:?}"
    );
    assert_eq!(
        Client::connect(&source).request(1, Op::Ping),
        Outcome::Ok(Answer::Pong)
    );
    assert_eq!(table_count(&runtime, "runtime_writer"), "0");
    assert!(!fixture.home().join("host/upgrades").exists());
    assert!(
        !runtime
            .with_extension("pre-codec-bootstrap.sqlite3")
            .exists()
    );
    rusqlite::Connection::open(&runtime)
        .unwrap()
        .execute_batch("DROP TABLE unexpected_runtime_shape")
        .unwrap();

    let upgraded = upgrade();
    assert!(upgraded.status.success(), "{upgraded:?}");
    assert_eq!(table_count(&runtime, "runtime_writer"), "1");
    assert!(
        runtime
            .with_extension("pre-codec-bootstrap.sqlite3")
            .is_file()
    );
    assert_eq!(
        query(
            &runtime,
            "SELECT hex(validation_seal) FROM checkpoints LIMIT 1"
        ),
        baseline
    );
    assert!(
        Client::connect_at(&source.socket)
            .rows(1)
            .iter()
            .any(|row| row.session == session)
    );
    stop(&mut source);
}

fn query(path: &Path, sql: &str) -> String {
    rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap()
        .query_row(sql, [], |row| row.get(0))
        .unwrap()
}

fn table_count(path: &Path, table: &str) -> String {
    query(
        path,
        &format!("SELECT CAST(count(*) AS TEXT) FROM sqlite_schema WHERE name='{table}'"),
    )
}

fn make_pre_bootstrap(path: &Path, epoch: u32) {
    let ddl = query(
        path,
        "SELECT sql FROM sqlite_schema WHERE name='runtime_objects'",
    );
    let legacy = ddl.replace(
        "codec INTEGER NOT NULL,",
        &format!("codec INTEGER NOT NULL CHECK(codec={epoch}),"),
    );
    assert_ne!(legacy, ddl);
    let connection = rusqlite::Connection::open(path).unwrap();
    connection
        .execute_batch(&format!(
            "PRAGMA foreign_keys=OFF; PRAGMA legacy_alter_table=ON;
         BEGIN IMMEDIATE; ALTER TABLE runtime_objects RENAME TO runtime_objects_fixture;
         {legacy};
         INSERT INTO runtime_objects SELECT * FROM runtime_objects_fixture;
         DROP TABLE runtime_objects_fixture; DROP TABLE runtime_writer; COMMIT;"
        ))
        .unwrap();
}
