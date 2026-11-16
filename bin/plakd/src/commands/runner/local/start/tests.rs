use super::*;

#[test]
fn a_start_probe_does_not_recreate_a_deleted_home() {
    let root = tempfile::tempdir().expect("root");
    let home = root.path().join("deleted-home");
    assert!(lock_free(&home, &home.join("host/runner/v3/run/runner.sock")).is_err());
    assert!(!home.exists());
}

#[test]
fn a_detached_starter_reaps_exited_children() {
    let child = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .expect("child");
    let pid = nix::unistd::Pid::from_raw(child.id().cast_signed());
    let pending = Arc::new(AtomicBool::new(false));
    reap(child, &pending);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while pending.load(Ordering::Acquire) {
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert!(
        nix::sys::signal::kill(pid, None).is_err(),
        "an unreaped child still exists"
    );
}
