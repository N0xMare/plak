//! Process-group termination and setup-failure cleanup.

#[cfg(target_os = "macos")]
mod macos_group_snapshot;
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
            #[cfg(target_os = "linux")]
            {
                self.count = live_group_members(self.group);
            }
            #[cfg(target_os = "macos")]
            {
                // The first query follows leader-exit observation. A snapshot
                // from before that query could still count the live leader,
                // delaying a short command by an entire polling interval.
                // Resetting `checked` for KILL also fences the new phase.
                let fresh_since = self.checked.unwrap_or_else(Instant::now);
                self.count = macos_group_snapshot::members(self.group, fresh_since).unwrap_or(1);
            }
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
#[cfg(target_os = "linux")]
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

    const HELPER_ENV: &str = "PLAKOUS_NON_UTF8_COMM_HELPER";

    /// A comm that is not UTF-8 is still a live member. `read_to_string` used
    /// to skip that stat, so the group looked empty while the process lived.
    ///
    /// The sleeper is this test binary, not the host `sleep`. A multi-call
    /// `sleep` (busybox, or coreutils built as one binary) is only a symlink;
    /// copying it dispatches on `argv[0]`, finds no applet, and exits before
    /// the scan.
    #[test]
    fn non_utf8_comm_is_counted_as_a_live_member() {
        if std::env::var_os(HELPER_ENV).is_some() {
            std::thread::sleep(std::time::Duration::from_secs(60));
            return;
        }
        let directory = tempfile::tempdir().expect("temp dir");
        // Linux truncates comm to 15 bytes, splitting the last character of
        // this valid 16-byte UTF-8 executable name.
        let program = directory.path().join("é".repeat(8));
        let source = std::env::current_exe().expect("test executable");
        std::fs::copy(&source, &program).expect("copy test executable");
        let stdout_path = directory.path().join("helper-stdout");
        let stderr_path = directory.path().join("helper-stderr");
        let stdout = std::fs::File::create(&stdout_path).expect("stdout");
        let stderr = std::fs::File::create(&stderr_path).expect("stderr");
        let filter = libtest_filter();
        let child = Command::new(&program)
            .arg("--exact")
            .arg(&filter)
            .arg("--test-threads=1")
            .env(HELPER_ENV, "1")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("spawn");
        let mut reap = Reap(Some(child));
        let child = reap.0.as_mut().expect("child");
        let id = child.id();
        let pid = i32::try_from(id)
            .ok()
            .and_then(rustix::process::Pid::from_raw)
            .expect("pid");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut observed = false;
        while std::time::Instant::now() < deadline {
            let status = child.try_wait().expect("wait");
            assert!(
                status.is_none(),
                "helper exited {status:?} before a non-UTF-8 comm was live; filter {filter}; stdout {}; stderr {}",
                read_text(&stdout_path),
                read_text(&stderr_path),
            );
            if non_utf8_comm(id) && super::live_group_members(pid) == 1 {
                observed = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let stat = std::fs::read(format!("/proc/{id}/stat")).unwrap_or_default();
        assert!(
            observed,
            "non-UTF-8 comm was not a live member; stat {stat:?}; filter {filter}; stdout {}; stderr {}",
            read_text(&stdout_path),
            read_text(&stderr_path),
        );
        let split = stat
            .iter()
            .rposition(|byte| *byte == b')')
            .expect("comm closer");
        let comm_at = stat.iter().position(|byte| *byte == b'(').expect("comm");
        assert!(std::str::from_utf8(&stat[comm_at + 1..split]).is_err());
        assert_eq!(super::live_group_members(pid), 1);
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(super::live_group_members(pid), 0);
    }

    /// Libtest's `--exact` filter is the test path without the crate name.
    fn libtest_filter() -> String {
        if let Some(name) = std::thread::current().name()
            && name.ends_with("non_utf8_comm_is_counted_as_a_live_member")
        {
            return name.to_owned();
        }
        let module = module_path!();
        let module = module.split_once("::").map_or(module, |(_, rest)| rest);
        format!("{module}::non_utf8_comm_is_counted_as_a_live_member")
    }

    fn non_utf8_comm(id: u32) -> bool {
        let Ok(stat) = std::fs::read(format!("/proc/{id}/stat")) else {
            return false;
        };
        let Some(split) = stat.iter().rposition(|byte| *byte == b')') else {
            return false;
        };
        let Some(comm_at) = stat.iter().position(|byte| *byte == b'(') else {
            return false;
        };
        comm_at < split && std::str::from_utf8(&stat[comm_at + 1..split]).is_err()
    }

    fn read_text(path: &std::path::Path) -> String {
        String::from_utf8_lossy(&std::fs::read(path).unwrap_or_default()).into_owned()
    }
}

pub(in crate::environment::backend) fn process_group_exists(
    process_group: rustix::process::Pid,
) -> bool {
    match rustix::process::test_kill_process_group(process_group) {
        Ok(()) | Err(Errno::PERM) => true,
        Err(_) => false,
    }
}
