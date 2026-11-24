use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};

use rustix::process::{Pid, WaitOptions, waitpid};
use tokio::io::{AsyncBufReadExt as _, BufReader};

#[tokio::test]
#[allow(clippy::unwrap_used, reason = "status cancellation fixture")]
async fn dropping_pending_ssh_status_reaps_its_child() {
    let (parent, child) = UnixStream::pair().unwrap();
    parent.set_nonblocking(true).unwrap();
    let mut reader = BufReader::new(tokio::net::UnixStream::from_std(parent).unwrap());
    let mut command = Command::new("sh");
    command
        .args(["-c", "printf '%s\\n' \"$$\"; exec cat"])
        .stdin(Stdio::from(OwnedFd::from(child.try_clone().unwrap())))
        .stdout(Stdio::from(OwnedFd::from(child)));
    let mut pid = String::new();
    {
        let mut status = std::pin::pin!(super::status(&mut command));
        tokio::select! {
            result = &mut status => panic!("child exited before its input closed: {result:?}"),
            read = reader.read_line(&mut pid) => assert!(read.unwrap() > 0),
        }
    }
    let pid = Pid::from_raw(pid.trim().parse().unwrap()).unwrap();
    assert!(matches!(
        waitpid(Some(pid), WaitOptions::NOHANG),
        Err(rustix::io::Errno::CHILD)
    ));
}

#[test]
#[allow(clippy::unwrap_used, reason = "launcher cancellation fixture")]
fn dropping_a_runner_launcher_reaps_without_its_runtime() {
    use rustix::process::test_kill_process;
    use std::time::{Duration, Instant};

    let (parent, child) = UnixStream::pair().unwrap();
    parent.set_nonblocking(true).unwrap();
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "printf '%s\\n' \"$$\"; exec cat"])
        .stdin(Stdio::from(OwnedFd::from(child.try_clone().unwrap())))
        .stdout(Stdio::from(OwnedFd::from(child)));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let pid = runtime.block_on(async {
        let mut reader = BufReader::new(tokio::net::UnixStream::from_std(parent).unwrap());
        let mut pid = String::new();
        let mut status = std::pin::pin!(super::launcher_status(&mut command));
        tokio::select! {
            result = &mut status => panic!("launcher exited before its deadline: {result:?}"),
            read = reader.read_line(&mut pid) => assert!(read.unwrap() > 0),
        }
        // Move the pipe out so its EOF is not what stops the launcher.
        (Pid::from_raw(pid.trim().parse().unwrap()).unwrap(), reader)
    });
    drop(runtime);
    let (pid, _reader) = pid;
    let deadline = Instant::now() + Duration::from_secs(2);
    while test_kill_process(pid) != Err(rustix::io::Errno::SRCH) {
        assert!(
            Instant::now() < deadline,
            "cancelled launcher still live or a zombie"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
