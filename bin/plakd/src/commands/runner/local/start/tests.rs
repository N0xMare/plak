use super::*;

#[test]
fn a_start_probe_does_not_recreate_a_deleted_home() {
    let root = tempfile::tempdir().expect("root");
    let home = root.path().join("deleted-home");
    assert!(lock_free(&home, &home.join("host/runner/v3/run/runner.sock")).is_err());
    assert!(!home.exists());
}

#[tokio::test]
async fn the_detached_daemon_is_not_a_host_child_but_stays_pending_until_it_exits() {
    use rustix::process::{Pid, WaitOptions, waitpid};

    let directory = tempfile::tempdir().expect("fixture");
    let release = directory.path().join("release");
    let pid_file = directory.path().join("pid");
    let mut command = detached_launcher(Path::new("/bin/sh"));
    command.args([
        "-c",
        "printf '%s\\n' \"$$\" > \"$1\"; n=0; while [ ! -f \"$2\" ] && [ \"$n\" -lt 500 ]; do n=$((n + 1)); sleep 0.01; done",
        "runner",
    ]).arg(&pid_file).arg(&release);
    let mut pending = Some(launch_detached(&mut command).await.expect("launch"));
    drop(command);
    let pid = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(raw) = std::fs::read_to_string(&pid_file)
                && let Ok(pid) = raw.trim().parse()
                && let Some(pid) = Pid::from_raw(pid)
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("daemon started");
    let direct_wait = waitpid(Some(pid), WaitOptions::NOHANG);
    let still_pending = detached_pending(&mut pending);
    std::fs::write(release, "").expect("release daemon");
    tokio::time::timeout(Duration::from_secs(5), async {
        while detached_pending(&mut pending) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("daemon exited");
    assert!(still_pending);
    assert!(matches!(direct_wait, Err(rustix::io::Errno::CHILD)));
}
