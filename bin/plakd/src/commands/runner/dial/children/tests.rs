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
