use std::time::{Duration, Instant};

use super::*;

/// A stand-in login shell: `body` is its whole script, whatever arguments
/// it is given.
fn shell(root: &Path, name: &str, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt as _;
    let path = root.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.display().to_string()
}

fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// A startup file that leaves a job in the background still holds the
/// pipe; the answer comes as soon as the shell exits, and the job is
/// killed with the shell's process group.
#[test]
fn a_background_job_neither_delays_the_answer_nor_outlives_it() {
    let root = tempfile::tempdir().unwrap();
    let pid_file = root.path().join("job.pid");
    let shell = shell(
        root.path(),
        "background",
        &format!(
            "sleep 20 &\necho $! > {}\nprintf %s /opt/tools/bin:/usr/bin",
            pid_file.display()
        ),
    );
    let started = Instant::now();
    let answer = login_shell_answer(&shell, Duration::from_secs(5));
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(answer.as_deref(), Some("/opt/tools/bin:/usr/bin"));
    let pid = std::fs::read_to_string(&pid_file).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid.trim()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!alive(pid.trim()), "the background job outlived the probe");
}

/// A shell that never answers is given up on at the deadline, with its
/// whole group; a failing or multi-line answer is no answer.
#[test]
fn a_shell_that_hangs_or_fails_gives_no_answer_within_the_deadline() {
    let root = tempfile::tempdir().unwrap();
    let hanging = shell(root.path(), "hanging", "sleep 20 &\nsleep 20");
    let started = Instant::now();
    assert_eq!(
        login_shell_answer(&hanging, Duration::from_millis(300)),
        None
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    let failing = shell(root.path(), "failing", "printf %s /usr/bin; exit 3");
    assert_eq!(login_shell_answer(&failing, Duration::from_secs(5)), None);
    let lines = shell(root.path(), "lines", "printf '/a\\n/b'");
    assert_eq!(login_shell_answer(&lines, Duration::from_secs(5)), None);
}
