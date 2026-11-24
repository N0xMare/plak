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

    let probe = paths.home().join(".tmpABCdef");
    fs::create_dir_all(probe.join("stores")).unwrap();
    fs::write(probe.join("stores/subagents.sqlite3"), b"scratch").unwrap();
    for name in [
        "runtime.sqlite3",
        "runtime.sqlite3-wal",
        "runtime.sqlite3-shm",
        "runtime.pre-codec-bootstrap.sqlite3",
    ] {
        fs::write(probe.join("stores").join(name), b"scratch").unwrap();
    }
    fs::write(probe.join("service.json"), b"{}").unwrap();
    let live = super::super::request_rehearsal(&probe).unwrap();
    let unrelated = paths.home().join(".tmp456abc/stores");
    fs::create_dir_all(&unrelated).unwrap();
    fs::write(unrelated.join("product.sqlite3"), b"not a probe").unwrap();

    stale_copies(&paths);
    assert!(!staging.exists());
    assert!(!abandoned_runtime.exists());
    assert!(active_runtime.exists());
    assert!(probe.join("stores/runtime.sqlite3").is_file());
    assert!(probe.join("stores/subagents.sqlite3").is_file());
    drop(live);
    stale_copies(&paths);
    assert!(!probe.exists());
    assert_eq!(fs::read(final_copy).unwrap(), b"published");
    assert_eq!(
        fs::read(unrelated.join("product.sqlite3")).unwrap(),
        b"not a probe"
    );
    assert_eq!(fs::read(runtime_copy).unwrap(), b"published runtime");
    assert_eq!(crate::store::revisions::backup_count(&paths), 2);
}
