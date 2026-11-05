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

const REGROUP: &str = "PLAK_TEST_REGROUPING_SHELL";
const REGROUP_CHILD: &str = "commands::runner::tests::regrouping_shell_child";

/// The stand-in shell's body: it moves itself into its parent's process
/// group, out of the one the probe made for it, and never answers.
#[test]
#[ignore = "process child entrypoint used only by the regrouping-shell probe"]
fn regrouping_shell_child() {
    if std::env::var_os(REGROUP).is_none() {
        return;
    }
    let parent = rustix::process::getppid().unwrap();
    let group = rustix::process::getpgid(Some(parent)).unwrap();
    rustix::process::setpgid(None, Some(group)).unwrap();
    std::thread::sleep(Duration::from_secs(20));
}

/// A shell that leaves the probe's process group escapes the group's kill;
/// it is still killed by pid, so the probe returns at its deadline.
#[test]
fn a_shell_that_leaves_its_group_is_still_given_up_on_at_the_deadline() {
    let root = tempfile::tempdir().unwrap();
    let shell = shell(
        root.path(),
        "regrouping",
        &format!(
            "{REGROUP}=1 exec {} --ignored --exact {REGROUP_CHILD} --nocapture --test-threads=1",
            std::env::current_exe().unwrap().display()
        ),
    );
    let started = Instant::now();
    assert_eq!(login_shell_answer(&shell, Duration::from_millis(500)), None);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
}
