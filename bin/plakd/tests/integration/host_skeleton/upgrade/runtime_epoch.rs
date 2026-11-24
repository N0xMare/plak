use super::*;
use std::path::Path;

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
    make_pre_bootstrap(&runtime);
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

fn make_pre_bootstrap(path: &Path) {
    let ddl = query(
        path,
        "SELECT sql FROM sqlite_schema WHERE name='runtime_objects'",
    );
    let legacy = ddl.replace(
        "codec INTEGER NOT NULL,",
        "codec INTEGER NOT NULL CHECK(codec=39),",
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
