//! Process-group termination and setup-failure cleanup.

#[cfg(target_os = "macos")]
use super::{Command, Stdio, set_nonblocking};
use super::{
    Duration, Errno, ExecutionBackendError, Instant, JoinedSessionShared, LeftoverJob, Mutex,
    io_error, lock, sleep_until_next_poll,
};

pub(super) fn terminate_owned_process_group(
    child: &mut std::process::Child,
    process_group: rustix::process::Pid,
    mut status: Option<std::process::ExitStatus>,
    grace: Duration,
) -> Result<std::process::ExitStatus, ExecutionBackendError> {
    let cleanup_deadline = Instant::now() + grace;
    terminate_owned_process_group_until(child, process_group, status.take(), cleanup_deadline)
}

pub(super) fn terminate_owned_process_group_until(
    child: &mut std::process::Child,
    process_group: rustix::process::Pid,
    status: Option<std::process::ExitStatus>,
    cleanup_deadline: Instant,
) -> Result<std::process::ExitStatus, ExecutionBackendError> {
    terminate_owned_process_group_with_grace(
        child,
        process_group,
        status,
        cleanup_deadline,
        Duration::from_secs(2),
    )
}

pub(super) fn terminate_owned_process_group_with_grace(
    child: &mut std::process::Child,
    process_group: rustix::process::Pid,
    mut status: Option<std::process::ExitStatus>,
    cleanup_deadline: Instant,
    grace: Duration,
) -> Result<std::process::ExitStatus, ExecutionBackendError> {
    let mut group = GroupProbe::new(process_group);
    let now = Instant::now();
    let term_deadline = now + grace.min(cleanup_deadline.saturating_duration_since(now) / 2);
    let _ = rustix::process::kill_process_group(process_group, rustix::process::Signal::TERM);
    while Instant::now() < term_deadline {
        if status.is_none()
            && peek_leader_exit(process_group).is_ok_and(|exit| exit.is_some())
            && group.members() == 0
        {
            status = child
                .try_wait()
                .map_err(|_| io_error("host process cleanup wait failed"))?;
        }
        if status.is_some() && group.members() == 0 {
            return status.ok_or_else(|| io_error("host process cleanup produced no status"));
        }
        sleep_until_next_poll(now, cleanup_deadline);
    }

    if process_group_exists(process_group) {
        let _ = rustix::process::kill_process_group(process_group, rustix::process::Signal::KILL);
        group.checked = None;
    }
    while Instant::now() < cleanup_deadline {
        if status.is_none()
            && peek_leader_exit(process_group).is_ok_and(|exit| exit.is_some())
            && group.members() == 0
        {
            status = child
                .try_wait()
                .map_err(|_| io_error("host process final join failed"))?;
        }
        if status.is_some() && group.members() == 0 {
            return status.ok_or_else(|| io_error("host process cleanup produced no status"));
        }
        sleep_until_next_poll(now, cleanup_deadline);
    }
    Err(io_error(
        "host process group cleanup exceeded its configured bound",
    ))
}

fn terminate_owned_pty_group(
    child: &mut dyn portable_pty::Child,
    process_group: rustix::process::Pid,
    status: Option<portable_pty::ExitStatus>,
    grace: Duration,
) -> Result<portable_pty::ExitStatus, ExecutionBackendError> {
    terminate_owned_pty_group_until(child, process_group, status, Instant::now() + grace)
}

pub(super) fn terminate_owned_pty_group_until(
    child: &mut dyn portable_pty::Child,
    process_group: rustix::process::Pid,
    status: Option<portable_pty::ExitStatus>,
    cleanup_deadline: Instant,
) -> Result<portable_pty::ExitStatus, ExecutionBackendError> {
    terminate_owned_pty_group_with_grace(
        child,
        process_group,
        status,
        cleanup_deadline,
        Duration::from_secs(2),
    )
}

pub(super) fn terminate_owned_pty_group_with_grace(
    child: &mut dyn portable_pty::Child,
    process_group: rustix::process::Pid,
    mut status: Option<portable_pty::ExitStatus>,
    cleanup_deadline: Instant,
    grace: Duration,
) -> Result<portable_pty::ExitStatus, ExecutionBackendError> {
    let mut group = GroupProbe::new(process_group);
    let now = Instant::now();
    let term_deadline = now + grace.min(cleanup_deadline.saturating_duration_since(now) / 2);
    let _ = rustix::process::kill_process_group(process_group, rustix::process::Signal::TERM);
    while Instant::now() < term_deadline {
        if status.is_none()
            && peek_leader_exit(process_group).is_ok_and(|exit| exit.is_some())
            && group.members() == 0
        {
            status = child
                .try_wait()
                .map_err(|_| io_error("joined process PTY cleanup wait failed"))?;
        }
        if status.is_some() && group.members() == 0 {
            return status.ok_or_else(|| io_error("joined process PTY cleanup produced no status"));
        }
        sleep_until_next_poll(now, cleanup_deadline);
    }
    if process_group_exists(process_group) {
        let _ = rustix::process::kill_process_group(process_group, rustix::process::Signal::KILL);
        group.checked = None;
    }
    while Instant::now() < cleanup_deadline {
        if status.is_none()
            && peek_leader_exit(process_group).is_ok_and(|exit| exit.is_some())
            && group.members() == 0
        {
            status = child
                .try_wait()
                .map_err(|_| io_error("joined process PTY final join failed"))?;
        }
        if status.is_some() && group.members() == 0 {
            return status.ok_or_else(|| io_error("joined process PTY cleanup produced no status"));
        }
        sleep_until_next_poll(now, cleanup_deadline);
    }
    Err(io_error(
        "joined process PTY group cleanup exceeded its configured bound",
    ))
}

pub(super) fn cleanup_pty_setup_error(
    child: &mut dyn portable_pty::Child,
    process_group: rustix::process::Pid,
    grace: Duration,
    setup_error: ExecutionBackendError,
) -> ExecutionBackendError {
    terminate_owned_pty_group(child, process_group, None, grace)
        .err()
        .unwrap_or(setup_error)
}

pub(super) fn wait_for_output_readers_until(
    stdout: &std::thread::JoinHandle<()>,
    stderr: &std::thread::JoinHandle<()>,
    deadline: Instant,
) {
    let started = Instant::now();
    while Instant::now() < deadline && (!stdout.is_finished() || !stderr.is_finished()) {
        sleep_until_next_poll(started, deadline);
    }
}

pub(super) fn join_thread_until(thread: std::thread::JoinHandle<()>, deadline: Instant) -> bool {
    let started = Instant::now();
    while Instant::now() < deadline && !thread.is_finished() {
        sleep_until_next_poll(started, deadline);
    }
    thread.is_finished() && thread.join().is_ok()
}

pub(super) fn cleanup_setup_error(
    child: &mut std::process::Child,
    process_group: rustix::process::Pid,
    grace: Duration,
    setup_error: ExecutionBackendError,
) -> ExecutionBackendError {
    terminate_owned_process_group(child, process_group, None, grace)
        .err()
        .unwrap_or(setup_error)
}

pub(super) fn terminate_direct_child(
    child: &mut std::process::Child,
    grace: Duration,
) -> Result<(), ExecutionBackendError> {
    let _ = child.kill();
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if child
            .try_wait()
            .map_err(|_| io_error("host process rollback wait failed"))?
            .is_some()
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Err(io_error(
        "host process rollback exceeded its configured bound",
    ))
}

pub(super) fn terminate_direct_pty_child(
    child: &mut dyn portable_pty::Child,
    grace: Duration,
) -> Result<(), ExecutionBackendError> {
    let _ = child.kill();
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if child
            .try_wait()
            .map_err(|_| io_error("joined process PTY rollback wait failed"))?
            .is_some()
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Err(io_error(
        "joined process PTY rollback exceeded its configured bound",
    ))
}

pub(super) struct LeaderExit {
    pub(super) code: Option<i32>,
}

pub(super) fn peek_leader_exit(
    pid: rustix::process::Pid,
) -> rustix::io::Result<Option<LeaderExit>> {
    use rustix::process::{WaitId, WaitIdOptions, waitid};
    waitid(
        WaitId::Pid(pid),
        WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
    )
    .map(|status| {
        status.map(|status| LeaderExit {
            code: status.exit_status(),
        })
    })
}

pub(super) struct GroupProbe {
    pub(super) group: rustix::process::Pid,
    pub(super) checked: Option<Instant>,
    pub(super) count: usize,
}

impl GroupProbe {
    pub(super) fn new(group: rustix::process::Pid) -> Self {
        Self {
            group,
            checked: None,
            count: 0,
        }
    }

    pub(super) fn members(&mut self) -> usize {
        if !process_group_exists(self.group) {
            return 0;
        }
        if self
            .checked
            .is_none_or(|at| at.elapsed() >= Duration::from_millis(250))
        {
            self.count = live_group_members(self.group);
            self.checked = Some(Instant::now());
        }
        self.count
    }
}

pub(super) fn observe_leftover_group(
    group: &mut GroupProbe,
    code: Option<i32>,
    shared: &Mutex<JoinedSessionShared>,
) -> bool {
    if !process_group_exists(group.group) {
        return false;
    }
    let count = group.members();
    if count == 0 {
        return false;
    }
    let mut shared = lock(shared);
    let changed = shared.leftover.is_none();
    shared.leftover = Some(LeftoverJob {
        exit_code: code,
        processes: count,
    });
    if changed {
        shared
            .changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
        if let Some(changes) = &shared.terminal_changes {
            changes.send_modify(|revision| *revision = revision.wrapping_add(1));
        }
    }
    true
}

// Zombies cannot run or receive signals. In particular, an orphaned zombie
// waiting for the host's init must not keep a server job alive forever.
fn live_group_members(group: rustix::process::Pid) -> usize {
    if !process_group_exists(group) {
        return 0;
    }
    live_group_members_impl(group).unwrap_or(1)
}

#[cfg(target_os = "linux")]
fn live_group_members_impl(group: rustix::process::Pid) -> Option<usize> {
    let mut count = 0;
    // `/proc/<pid>/stat` is a single_open record: a successful read is the
    // whole line, never a truncated prefix. A directory-entry error ends
    // `ReadDir`, so the scan is unknown rather than a short count callers
    // would trust as empty. An unreadable field is the same. A missing stat
    // is a pid reaped between readdir and the read; that member is gone.
    for entry in std::fs::read_dir("/proc").ok()? {
        let entry = entry.ok()?;
        if !entry
            .file_name()
            .as_encoded_bytes()
            .iter()
            .all(u8::is_ascii_digit)
        {
            continue;
        }
        let Ok(stat) = std::fs::read(entry.path().join("stat")) else {
            continue;
        };
        let split = stat.iter().rposition(|byte| *byte == b')')?;
        let fields = std::str::from_utf8(&stat[split + 1..]).ok()?;
        let mut fields = fields.split_whitespace();
        let state = fields.next()?;
        let _parent = fields.next()?;
        let pgid = fields.next()?.parse::<i32>().ok()?;
        if pgid == group.as_raw_pid() && state != "Z" && state != "X" {
            count += 1;
        }
    }
    Some(count)
}

#[cfg(all(test, target_os = "linux"))]
#[allow(
    clippy::expect_used,
    reason = "the non-UTF-8 comm fixture fails the test rather than the scan"
)]
mod tests {
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};

    struct Reap(Option<Child>);

    impl Drop for Reap {
        fn drop(&mut self) {
            if let Some(child) = self.0.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    /// A comm that is not UTF-8 is still a live member. `read_to_string` used
    /// to skip that stat, so the group looked empty while the process lived.
    #[test]
    fn non_utf8_comm_is_counted_as_a_live_member() {
        let directory = tempfile::tempdir().expect("temp dir");
        // Linux truncates comm to 15 bytes, splitting the last character of
        // this valid 16-byte UTF-8 executable name.
        let program = directory.path().join("é".repeat(8));
        let source = ["/bin/sleep", "/usr/bin/sleep"]
            .into_iter()
            .find(|candidate| std::path::Path::new(candidate).is_file())
            .expect("sleep");
        std::fs::copy(source, &program).expect("copy sleep");
        let child = Command::new(&program)
            .arg("60")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        let mut reap = Reap(Some(child));
        let id = reap.0.as_ref().expect("child").id();
        let pid = i32::try_from(id)
            .ok()
            .and_then(rustix::process::Pid::from_raw)
            .expect("pid");
        let stat = std::fs::read(format!("/proc/{id}/stat")).expect("stat");
        let split = stat
            .iter()
            .rposition(|byte| *byte == b')')
            .expect("comm closer");
        let comm_at = stat.iter().position(|byte| *byte == b'(').expect("comm");
        assert!(std::str::from_utf8(&stat[comm_at + 1..split]).is_err());
        assert_eq!(super::live_group_members(pid), 1);
        if let Some(mut child) = reap.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        assert_eq!(super::live_group_members(pid), 0);
    }
}

#[cfg(target_os = "macos")]
fn live_group_members_impl(group: rustix::process::Pid) -> Option<usize> {
    let mut child = Command::new("/bin/ps")
        .args(["-axo", "pgid=,stat="])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut output = Vec::new();
    let collected = (|| {
        let mut stdout = child.stdout.take()?;
        set_nonblocking(&stdout).ok()?;
        let deadline = Instant::now() + Duration::from_millis(100);
        let mut buffer = [0; 4096];
        loop {
            match std::io::Read::read(&mut stdout, &mut buffer) {
                Ok(0) => break,
                Ok(count) => output.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return None,
            }
            if output.len() > 1_048_576 || Instant::now() >= deadline {
                return None;
            }
        }
        loop {
            if let Some(status) = child.try_wait().ok()? {
                return status.success().then_some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    })();
    if collected.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    Some(
        String::from_utf8(output)
            .ok()?
            .lines()
            .filter(|line| {
                let mut fields = line.split_whitespace();
                fields.next().and_then(|pgid| pgid.parse::<i32>().ok()) == Some(group.as_raw_pid())
                    && fields.next().is_some_and(|state| !state.starts_with('Z'))
            })
            .count(),
    )
}

pub(in crate::environment::backend) fn process_group_exists(
    process_group: rustix::process::Pid,
) -> bool {
    match rustix::process::test_kill_process_group(process_group) {
        Ok(()) | Err(Errno::PERM) => true,
        Err(_) => false,
    }
}
