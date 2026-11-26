#![allow(clippy::unwrap_used, reason = "unit tests")]

use super::*;

#[test]
fn startup_discards_abandoned_copies_but_keeps_live_and_final_copies() {
    let root = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("home"));
    paths.ensure_layout().unwrap();
    let state = paths.state_dir();
    let staging = state.join(".product-backup-844bbfb9-6dcb-41d8-8ece-be1234a63067");
    fs::create_dir(&staging).unwrap();
    fs::write(staging.join("copy.sqlite3"), b"unpublished").unwrap();
    let final_copy = state.join("product.pre-revision-39.sqlite3");
    fs::write(&final_copy, b"published").unwrap();
    let runtime_copy = state.join("runtime.pre-codec-bootstrap.sqlite3");
    fs::write(&runtime_copy, b"published runtime").unwrap();
    let abandoned_runtime = state.join(".runtime-bootstrap-2147483647-0");
    fs::create_dir(&abandoned_runtime).unwrap();
    fs::write(abandoned_runtime.join("copy.sqlite3-journal"), b"scratch").unwrap();
    let active_runtime = state.join(format!(".runtime-bootstrap-{}-0", std::process::id()));
    fs::create_dir(&active_runtime).unwrap();

    stale_copies(&paths);
    assert!(!staging.exists());
    assert!(!abandoned_runtime.exists());
    assert!(active_runtime.exists());
    assert_eq!(fs::read(final_copy).unwrap(), b"published");
    assert_eq!(fs::read(runtime_copy).unwrap(), b"published runtime");
    assert_eq!(crate::store::revisions::backup_count(&paths), 2);
}

#[test]
fn startup_reclaims_only_unlocked_legacy_probe_copies() {
    let (_temporary, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().unwrap();
    let probe = |name: &str| {
        let path = paths.home().join(name);
        fs::create_dir_all(path.join("stores")).unwrap();
        fs::write(path.join("stores/product.sqlite3-wal"), []).unwrap();
        fs::write(path.join("scratch-request"), []).unwrap();
        path
    };
    let abandoned = probe(".tmp123abc");
    let live = probe(".tmp321abc");
    let held = lock_marker(&live.join("scratch-request"), false).unwrap();
    stale_copies(&paths);
    assert!(!abandoned.exists());
    assert!(live.exists());
    drop(held);
    stale_copies(&paths);
    assert!(!live.exists());
}
