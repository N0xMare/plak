//! Process-group termination and setup-failure cleanup.

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod group_snapshot;
mod reaper;
use super::{
    Duration, Errno, ExecutionBackendError, Instant, JoinedSessionShared, LeftoverJob, Mutex,
    io_error, lock, sleep_until_next_poll,
};
pub(in crate::environment::backend) use reaper::reap_when_group_empty;

pub(super) fn terminate_owned_process_group(
    child: &mut std::process::Child,
    process_group: rustix::process::Pid,
    grace: Duration,
) -> Result<std::process::ExitStatus, ExecutionBackendError> {
    let cleanup_deadline = Instant::now() + grace;
    terminate_owned_process_group_until(child, process_group, cleanup_deadline)
}

pub(super) fn terminate_owned_process_group_until(
    child: &mut std::process::Child,
    process_group: rustix::process::Pid,
    cleanup_deadline: Instant,
) -> Result<std::process::ExitStatus, ExecutionBackendError> {
    terminate_owned_process_group_with_grace(
        child,
        process_group,
        None,
        cleanup_deadline,
        Duration::from_secs(2),
    )
}

/// TERM, then KILL, the owned group, reaping its leader only once no live
/// member remains, so the unreaped leader pins the PGID through every signal.
///
/// A `status` means the caller already reaped the leader, which it does only
/// after proving the group empty: nothing is signalled to the unpinned PGID.
pub(super) fn terminate_owned_process_group_with_grace(
    child: &mut std::process::Child,
    process_group: rustix::process::Pid,
    status: Option<std::process::ExitStatus>,
    cleanup_deadline: Instant,
    grace: Duration,
) -> Result<std::process::ExitStatus, ExecutionBackendError> {
    if let Some(status) = status {
        return Ok(status);
    }
    let mut group = GroupProbe::new(process_group);
    let now = Instant::now();
    let term_deadline = now + grace.min(cleanup_deadline.saturating_duration_since(now) / 2);
    let _ = rustix::process::kill_process_group(process_group, rustix::process::Signal::TERM);
    while Instant::now() < term_deadline {
        if peek_leader_exit(process_group).is_ok_and(|exit| exit.is_some())
            && group.members() == 0
            && let Some(status) = child
                .try_wait()
                .map_err(|_| io_error("host process cleanup wait failed"))?
        {
            return Ok(status);
        }
        sleep_until_next_poll(now, cleanup_deadline);
    }

    if process_group_exists(process_group) {
        let _ = rustix::process::kill_process_group(process_group, rustix::process::Signal::KILL);
        group.checked = None;
    }
    while Instant::now() < cleanup_deadline {
        if peek_leader_exit(process_group).is_ok_and(|exit| exit.is_some())
            && group.members() == 0
            && let Some(status) = child
                .try_wait()
                .map_err(|_| io_error("host process final join failed"))?
        {
            return Ok(status);
        }
        sleep_until_next_poll(now, cleanup_deadline);
    }
    Err(io_error(
        "host process group cleanup exceeded its configured bound",
    ))
}

/// The PTY counterpart of [`terminate_owned_process_group_with_grace`], with
/// the same PGID pinning and the same meaning of `status`.
pub(super) fn terminate_owned_pty_group_with_grace(
    child: &mut dyn portable_pty::Child,
    process_group: rustix::process::Pid,
    status: Option<portable_pty::ExitStatus>,
    cleanup_deadline: Instant,
    grace: Duration,
) -> Result<portable_pty::ExitStatus, ExecutionBackendError> {
    if let Some(status) = status {
        return Ok(status);
    }
    let mut group = GroupProbe::new(process_group);
    let now = Instant::now();
    let term_deadline = now + grace.min(cleanup_deadline.saturating_duration_since(now) / 2);
    let _ = rustix::process::kill_process_group(process_group, rustix::process::Signal::TERM);
    while Instant::now() < term_deadline {
        if peek_leader_exit(process_group).is_ok_and(|exit| exit.is_some())
            && group.members() == 0
            && let Some(status) = child
                .try_wait()
                .map_err(|_| io_error("joined process PTY cleanup wait failed"))?
        {
            return Ok(status);
        }
        sleep_until_next_poll(now, cleanup_deadline);
    }
    if process_group_exists(process_group) {
        let _ = rustix::process::kill_process_group(process_group, rustix::process::Signal::KILL);
        group.checked = None;
    }
    while Instant::now() < cleanup_deadline {
        if peek_leader_exit(process_group).is_ok_and(|exit| exit.is_some())
            && group.members() == 0
            && let Some(status) = child
                .try_wait()
                .map_err(|_| io_error("joined process PTY final join failed"))?
        {
            return Ok(status);
        }
        sleep_until_next_poll(now, cleanup_deadline);
    }
    Err(io_error(
        "joined process PTY group cleanup exceeded its configured bound",
    ))
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

/// Terminates a child whose setup failed; one whose group outlives the
/// cleanup goes to the reaper.
pub(super) fn cleanup_setup_error(
    mut child: std::process::Child,
    process_group: rustix::process::Pid,
    grace: Duration,
    setup_error: ExecutionBackendError,
) -> ExecutionBackendError {
    match terminate_owned_process_group(&mut child, process_group, grace) {
        Ok(_) => setup_error,
        Err(error) => {
            reap_when_group_empty(process_group, Box::new(child));
            error
        }
    }
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

/// Whether the unreaped `leader` has exited; an error reads as not yet.
pub(in crate::environment::backend) fn leader_exited(leader: rustix::process::Pid) -> bool {
    peek_leader_exit(leader).is_ok_and(|exit| exit.is_some())
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

pub(in crate::environment::backend) struct GroupProbe {
    pub(super) group: rustix::process::Pid,
    pub(super) checked: Option<Instant>,
    pub(super) count: usize,
}

impl GroupProbe {
    pub(in crate::environment::backend) fn new(group: rustix::process::Pid) -> Self {
        Self {
            group,
            checked: None,
            count: 0,
        }
    }

    pub(in crate::environment::backend) fn members(&mut self) -> usize {
        match rustix::process::test_kill_process_group(self.group) {
            // Linux answers Ok for a group that holds only our unreaped
            // leader, so EPERM means only another user's members remain, and
            // a hardened `/proc` (`hidepid`, `subset=pid`) may hide them from
            // the scan: the count is unknown, which counts as live. macOS
            // answers EPERM for that zombie-only group, and its `ps` scan
            // lists every user's processes, so the scan decides.
            Err(Errno::PERM) if cfg!(target_os = "linux") => return self.count.max(1),
            Ok(()) | Err(Errno::PERM) => {}
            Err(_) => return 0,
        }
        if self
            .checked
            .is_none_or(|at| at.elapsed() >= Duration::from_millis(250))
        {
            // The first query follows leader-exit observation. A snapshot
            // from before that query could still count the live leader,
            // delaying a short command by an entire polling interval.
            // Resetting `checked` for KILL also fences the new phase.
            let fresh_since = self.checked.unwrap_or_else(Instant::now);
            self.count = group_snapshot::members(self.group, fresh_since).unwrap_or(1);
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

#[cfg(all(test, target_os = "linux"))]
#[allow(
    clippy::expect_used,
    reason = "a fixture that cannot be set up fails the test rather than the probe"
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

    /// Kills a background member that outlives its reaped leader.
    struct KillMember(rustix::process::Pid);

    impl Drop for KillMember {
        fn drop(&mut self) {
            let _ = rustix::process::kill_process(self.0, rustix::process::Signal::KILL);
        }
    }

    /// Once the leader is reaped its PGID is no longer pinned and could name
    /// another user's group, so termination given the reaped status signals
    /// nothing, even to a member that is still running.
    #[test]
    fn a_reaped_leader_status_signals_nothing() {
        let directory = tempfile::tempdir().expect("temp dir");
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 30 & echo $! > member.pid")
            .current_dir(directory.path())
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        let group = i32::try_from(child.id())
            .ok()
            .and_then(rustix::process::Pid::from_raw)
            .expect("pid");
        let status = child.wait().expect("leader exit");
        let member = std::fs::read_to_string(directory.path().join("member.pid"))
            .ok()
            .and_then(|pid| pid.trim().parse::<i32>().ok())
            .and_then(rustix::process::Pid::from_raw)
            .expect("member pid");
        let _member = KillMember(member);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let returned = super::terminate_owned_process_group_with_grace(
            &mut child,
            group,
            Some(status),
            deadline,
            std::time::Duration::from_millis(200),
        )
        .expect("reaped status");
        assert_eq!(returned, status);
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(live_group_members(group), 1);
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
            if non_utf8_comm(id) && live_group_members(pid) == 1 {
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
        assert_eq!(live_group_members(pid), 1);
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(live_group_members(pid), 0);
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

    fn live_group_members(group: rustix::process::Pid) -> usize {
        super::group_snapshot::live_groups()
            .expect("complete /proc scan")
            .get(&group.as_raw_pid())
            .copied()
            .unwrap_or(0)
    }

    fn read_text(path: &std::path::Path) -> String {
        String::from_utf8_lossy(&std::fs::read(path).unwrap_or_default()).into_owned()
    }
}

pub(super) fn process_group_exists(process_group: rustix::process::Pid) -> bool {
    match rustix::process::test_kill_process_group(process_group) {
        Ok(()) | Err(Errno::PERM) => true,
        Err(_) => false,
    }
}
