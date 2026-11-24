use super::*;

#[test]
fn a_start_probe_does_not_recreate_a_deleted_home() {
    let root = tempfile::tempdir().expect("root");
    let home = root.path().join("deleted-home");
    assert!(lock_free(&home, &home.join("host/runner/v3/run/runner.sock")).is_err());
    assert!(!home.exists());
}
