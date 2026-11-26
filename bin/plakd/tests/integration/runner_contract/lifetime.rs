//! A real detached stdio runner must not survive a test killed by `SIGKILL`.

use std::fs;
use std::os::unix::process::ExitStatusExt as _;
use std::process::Stdio;

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::process::{Pid, PidfdFlags, Signal, getpgid, pidfd_open, pidfd_send_signal};

use super::*;
use crate::daemon_lifetime::Lifetime;
use crate::offline_support::{TestChild, WAIT, opened_by};

const ROOT: &str = "PLAK_TEST_SIGKILL_FIXTURE_ROOT";

#[test]
fn sigkill_reaps_a_tests_detached_runner() {
    if let Some(root) = std::env::var_os(ROOT) {
        own_runner(Path::new(&root));
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let mut owner = TestChild::spawn(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runner_contract::lifetime::sigkill_reaps_a_tests_detached_runner",
            ])
            .env(ROOT, root.path()),
    );
    let deadline = Instant::now() + WAIT;
    let daemon = loop {
        if let Ok(pid) = fs::read_to_string(root.path().join("ready"))
            && let Ok(pid) = pid.parse::<i32>()
        {
            break Pid::from_raw(pid).unwrap();
        }
        assert!(
            owner.try_wait().unwrap().is_none(),
            "test owner exited before readiness"
        );
        assert!(Instant::now() < deadline, "detached runner did not serve");
        std::thread::sleep(Duration::from_millis(10));
    };
    let pidfd = pidfd_open(daemon, PidfdFlags::empty()).unwrap();
    assert_eq!(getpgid(Some(daemon)).unwrap(), daemon);
    assert_ne!(daemon.as_raw_nonzero().get().cast_unsigned(), owner.id());
    owner.kill().unwrap();
    assert_eq!(owner.wait().unwrap().signal(), Some(9));

    let exited = poll(
        &mut [PollFd::new(&pidfd, PollFlags::IN)],
        Some(&Timespec {
            tv_sec: 15,
            tv_nsec: 0,
        }),
    );
    if !matches!(exited, Ok(1)) {
        // A failing test must not itself leave the process behind.
        let _ = pidfd_send_signal(&pidfd, Signal::KILL);
    }
    assert_eq!(
        exited.unwrap(),
        1,
        "detached runner survived the test's SIGKILL"
    );
}

fn own_runner(root: &Path) {
    let _lifetime = Lifetime::new(root);
    let home = root.join("home");
    let work = root.join("work");
    fs::create_dir_all(home.join("runner")).unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir(&work).unwrap();
    fs::write(
        home.join("runner/runner.toml"),
        format!(
            "roots = [{:?}]\npath = \"/usr/bin:/bin\"\nidle_exit_seconds = 300\n",
            work.to_str().unwrap()
        ),
    )
    .unwrap();
    let mut bridge = TestChild::spawn(
        plak_command(&home)
            .args(["runner", "connect", "--stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null()),
    );
    let deadline = Instant::now() + WAIT;
    loop {
        for build in fs::read_dir(home.join("runner/builds"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let run = build.path().join("run");
            if std::os::unix::net::UnixStream::connect(run.join("s.sock")).is_ok()
                && let [pid] = opened_by(&run.join("s.lock")).as_slice()
            {
                let ready = root.join("ready.pending");
                fs::write(&ready, pid.to_string()).unwrap();
                fs::rename(ready, root.join("ready")).unwrap();
                loop {
                    std::thread::park();
                }
            }
        }
        assert!(
            bridge.try_wait().unwrap().is_none(),
            "runner bridge exited before readiness"
        );
        assert!(
            Instant::now() < deadline,
            "runner did not publish its socket and lock"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
