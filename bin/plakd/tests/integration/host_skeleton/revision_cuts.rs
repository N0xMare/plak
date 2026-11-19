use std::time::{Duration, Instant};

use super::*;

const BASE: [i64; 2] = [39, 11];
const CURRENT: [i64; 2] = [40, 12];

#[test]
fn product_revision_crash_before_commit_leaves_both_stores_openable_by_the_source() {
    crash("product:before-commit", BASE);
}

#[test]
fn product_revision_crash_after_commit_restarts_a_mixed_revision_home() {
    crash("product:after-commit", [40, 11]);
}

#[test]
fn tree_revision_crash_before_commit_restarts_a_mixed_revision_home() {
    crash("subagents:before-commit", [40, 11]);
}

#[test]
fn tree_revision_crash_after_commit_skips_both_steps_and_the_old_host_refuses_tree_only() {
    crash("subagents:after-commit", CURRENT);
}

fn crash(cut: &str, expected: [i64; 2]) {
    let Some(fixture) = fixture() else { return };
    let image = PathBuf::from(env!("CARGO_BIN_EXE_plakd"))
        .canonicalize()
        .unwrap();
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let session = upgrade::seed(&fixture, 1);
    stop(&mut start(&fixture));
    assert_eq!(versions(&paths), BASE);

    let (child, lines) = spawn_env(
        &fixture,
        &[],
        &[
            (
                "PLAK_TEST_STORE_REVISION_IMAGE",
                image.to_str().unwrap().to_owned(),
            ),
            ("PLAK_TEST_HOST_UPGRADE_CUT", cut.to_owned()),
        ],
    );
    let (code, stderr) = exit(child, &lines);
    assert_eq!(code, Some(86), "{cut}: {stderr}");
    assert_eq!(versions(&paths), expected);
    let product_backup = paths.state_dir().join("product.pre-revision-39.sqlite3");
    let tree_backup = paths.state_dir().join("subagents.pre-revision-11.sqlite3");
    for (backup, revision) in [(&product_backup, BASE[0]), (&tree_backup, BASE[1])] {
        assert_eq!(version(backup), revision);
    }
    if expected == BASE {
        stop(&mut start(&fixture));
    } else {
        older_refuses_unchanged(&fixture, &paths, "product schema 40");
    }
    let product_copy_time = product_backup.metadata().unwrap().modified().unwrap();
    let tree_copy_time = tree_backup.metadata().unwrap().modified().unwrap();
    let mut environment = vec![(
        "PLAK_TEST_STORE_REVISION_IMAGE",
        image.to_str().unwrap().to_owned(),
    )];
    // Reusing the cut on a committed revision must not reach it again.
    if cut.ends_with(":after-commit") {
        environment.push(("PLAK_TEST_HOST_UPGRADE_CUT", cut.to_owned()));
    }
    let mut resumed = start_env(&fixture, &[], &environment);
    assert_eq!(versions(&paths), CURRENT);
    assert!(
        Client::connect(&resumed)
            .rows(1)
            .iter()
            .any(|row| row.session == session)
    );
    stop(&mut resumed);
    if expected[0] == CURRENT[0] {
        assert_eq!(
            product_backup.metadata().unwrap().modified().unwrap(),
            product_copy_time
        );
    }
    if expected[1] == CURRENT[1] {
        assert_eq!(
            tree_backup.metadata().unwrap().modified().unwrap(),
            tree_copy_time
        );
        std::fs::copy(product_backup, paths.product_database()).unwrap();
        assert_eq!(versions(&paths), [39, 12]);
        older_refuses_unchanged(&fixture, &paths, "subagents schema 12");
    }
}

fn exit(mut child: Child, lines: &mpsc::Receiver<String>) -> (Option<i32>, String) {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return (status.code(), lines.iter().collect::<Vec<_>>().join("\n"));
        }
        assert!(
            Instant::now() < deadline,
            "host did not exit: {:?}",
            lines.try_iter().collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn version(path: &std::path::Path) -> i64 {
    rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap()
}

fn versions(paths: &plakd::home::paths::PlakPaths) -> [i64; 2] {
    [
        version(&paths.product_database()),
        version(&paths.subagents_database()),
    ]
}

fn older_refuses_unchanged(fixture: &Fixture, paths: &plakd::home::paths::PlakPaths, reason: &str) {
    let stores = [
        paths.runtime_database(),
        paths.product_database(),
        paths.subagents_database(),
    ];
    let before = stores.each_ref().map(|path| store_bytes(path));
    let (code, stderr) = {
        let (child, lines) = spawn(fixture);
        exit(child, &lines)
    };
    assert_ne!(code, Some(0), "{stderr}");
    assert!(
        stderr.contains(reason) && stderr.contains("incompatible plakd build"),
        "{stderr}"
    );
    assert_eq!(stores.each_ref().map(|path| store_bytes(path)), before);
}

fn store_bytes(path: &std::path::Path) -> (Vec<u8>, Vec<u8>) {
    let wal = path.with_extension("sqlite3-wal");
    (
        std::fs::read(path).unwrap(),
        std::fs::read(wal).unwrap_or_default(),
    )
}
