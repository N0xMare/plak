//! Process resource sampling for sidecar parent clients.

use std::time::Duration;

/// Content-free resource snapshot.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProcessResourceUsage {
    pub user_cpu_ns: u64,
    pub system_cpu_ns: u64,
    pub peak_rss_bytes: Option<u64>,
    pub rss_bytes: Option<u64>,
    pub threads: Option<u64>,
    pub fds: Option<u64>,
}

#[cfg(unix)]
fn timeval_duration(secs: u64, micros: u64) -> Duration {
    Duration::from_secs(secs).saturating_add(Duration::from_micros(micros))
}

#[cfg(target_os = "linux")]
fn normalize_max_rss_bytes(raw: u64) -> u64 {
    raw.saturating_mul(1024)
}

#[cfg(not(target_os = "linux"))]
fn normalize_max_rss_bytes(raw: u64) -> u64 {
    raw
}

/// Samples cumulative reaped-child usage (`RUSAGE_CHILDREN`).
#[must_use]
pub fn sample_children_rusage() -> ProcessResourceUsage {
    let mut usage = ProcessResourceUsage::default();
    #[cfg(unix)]
    {
        use nix::sys::resource::{UsageWho, getrusage};
        if let Ok(rusage) = getrusage(UsageWho::RUSAGE_CHILDREN) {
            let user = rusage.user_time();
            let system = rusage.system_time();
            let user_cpu = timeval_duration(
                user.tv_sec().try_into().unwrap_or(0),
                user.tv_usec().try_into().unwrap_or(0),
            );
            let system_cpu = timeval_duration(
                system.tv_sec().try_into().unwrap_or(0),
                system.tv_usec().try_into().unwrap_or(0),
            );
            usage.user_cpu_ns = u64::try_from(user_cpu.as_nanos()).unwrap_or(u64::MAX);
            usage.system_cpu_ns = u64::try_from(system_cpu.as_nanos()).unwrap_or(u64::MAX);
            let max_rss = rusage.max_rss();
            if max_rss > 0 {
                usage.peak_rss_bytes =
                    Some(normalize_max_rss_bytes(u64::try_from(max_rss).unwrap_or(0)));
            }
        }
    }
    usage
}

/// Delta between two `RUSAGE_CHILDREN` samples (valid when reaps are serialized).
#[must_use]
pub fn children_rusage_delta(
    before: ProcessResourceUsage,
    after: ProcessResourceUsage,
) -> ProcessResourceUsage {
    ProcessResourceUsage {
        user_cpu_ns: after.user_cpu_ns.saturating_sub(before.user_cpu_ns),
        system_cpu_ns: after.system_cpu_ns.saturating_sub(before.system_cpu_ns),
        peak_rss_bytes: match (before.peak_rss_bytes, after.peak_rss_bytes) {
            (None | Some(0), Some(a)) if a > 0 => Some(a),
            (Some(b), Some(a)) if b == 0 && a > 0 => Some(a),
            _ => None,
        },
        rss_bytes: after.rss_bytes,
        threads: after.threads,
        fds: after.fds,
    }
}

/// Samples this process via `RUSAGE_SELF` (sidecar worker self-report).
#[must_use]
pub fn sample_self_rusage() -> ProcessResourceUsage {
    let mut usage = ProcessResourceUsage::default();
    #[cfg(unix)]
    {
        use nix::sys::resource::{UsageWho, getrusage};
        if let Ok(rusage) = getrusage(UsageWho::RUSAGE_SELF) {
            let user = rusage.user_time();
            let system = rusage.system_time();
            let user_cpu = timeval_duration(
                user.tv_sec().try_into().unwrap_or(0),
                user.tv_usec().try_into().unwrap_or(0),
            );
            let system_cpu = timeval_duration(
                system.tv_sec().try_into().unwrap_or(0),
                system.tv_usec().try_into().unwrap_or(0),
            );
            usage.user_cpu_ns = u64::try_from(user_cpu.as_nanos()).unwrap_or(u64::MAX);
            usage.system_cpu_ns = u64::try_from(system_cpu.as_nanos()).unwrap_or(u64::MAX);
            let max_rss = rusage.max_rss();
            if max_rss > 0 {
                usage.peak_rss_bytes =
                    Some(normalize_max_rss_bytes(u64::try_from(max_rss).unwrap_or(0)));
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            let mut rss_kb = 0_u64;
            let mut threads = 0_u64;
            for line in status.lines() {
                let mut parts = line.split_whitespace();
                let Some(key) = parts.next() else {
                    continue;
                };
                let Some(value) = parts.next().and_then(|value| value.parse::<u64>().ok()) else {
                    continue;
                };
                match key {
                    "VmRSS:" => rss_kb = value,
                    "Threads:" => threads = value,
                    _ => {}
                }
            }
            if rss_kb > 0 {
                usage.rss_bytes = Some(rss_kb.saturating_mul(1024));
            }
            if threads > 0 {
                usage.threads = Some(threads);
            }
        }
        usage.fds = std::fs::read_dir("/proc/self/fd")
            .ok()
            .and_then(|entries| u64::try_from(entries.count()).ok());
    }
    usage
}

/// How far a bounded reap had to escalate before the child was gone.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ChildReapEscalation {
    /// The child exited on its own inside the voluntary bound.
    #[default]
    Voluntary,
    /// The child was still alive at the voluntary bound and took `SIGTERM`.
    Terminated,
    /// `SIGTERM` was ignored and the child took `SIGKILL`.
    Killed,
    /// The child was still not reaped after `SIGKILL` and the final bound.
    Unreaped,
}

/// Documented bounds for one escalating reap. Every phase is finite, so a
/// worker that never exits cannot hold shutdown open.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChildReapBounds {
    /// How long a child may take to exit on its own before `SIGTERM`.
    pub voluntary: Duration,
    /// How long `SIGTERM` may take to work before `SIGKILL`.
    pub terminate: Duration,
    /// How long `SIGKILL` may take to be reaped before the reap gives up.
    pub kill: Duration,
}

impl ChildReapBounds {
    /// The shutdown bounds local composition uses: at most 14 s in total.
    pub const DEFAULT: Self = Self {
        voluntary: Duration::from_secs(10),
        terminate: Duration::from_secs(2),
        kill: Duration::from_secs(2),
    };
}

impl Default for ChildReapBounds {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Poll interval for every bounded reap phase.
#[cfg(unix)]
const REAP_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Reaps `pid` within `bounds`, escalating `SIGTERM` then `SIGKILL`, and
/// reports how far it had to go. Never blocks indefinitely: a child that
/// ignores every signal returns [`ChildReapEscalation::Unreaped`] once the
/// final bound elapses rather than holding the caller open.
#[cfg(unix)]
#[must_use]
pub fn reap_child_pid_bounded(pid: u32, bounds: ChildReapBounds) -> ChildReapEscalation {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    let Ok(raw) = i32::try_from(pid) else {
        return ChildReapEscalation::Voluntary;
    };
    let pid = Pid::from_raw(raw);
    if reap_within(pid, bounds.voluntary) {
        return ChildReapEscalation::Voluntary;
    }
    let _ = kill(pid, Signal::SIGTERM);
    if reap_within(pid, bounds.terminate) {
        return ChildReapEscalation::Terminated;
    }
    let _ = kill(pid, Signal::SIGKILL);
    if reap_within(pid, bounds.kill) {
        ChildReapEscalation::Killed
    } else {
        ChildReapEscalation::Unreaped
    }
}

/// Polls `waitpid` until `pid` is reaped or `bound` elapses.
#[cfg(unix)]
fn reap_within(pid: nix::unistd::Pid, bound: Duration) -> bool {
    use nix::sys::wait::{WaitPidFlag, waitpid};
    reap_poll_within(bound, || waitpid(pid, Some(WaitPidFlag::WNOHANG)))
}

#[cfg(unix)]
fn reap_poll_within(
    bound: Duration,
    mut poll: impl FnMut() -> nix::Result<nix::sys::wait::WaitStatus>,
) -> bool {
    use nix::sys::wait::WaitStatus;
    let start = std::time::Instant::now();
    loop {
        match poll() {
            Ok(WaitStatus::Exited(..) | WaitStatus::Signaled(..))
            | Err(nix::errno::Errno::ECHILD) => return true,
            Ok(_) | Err(nix::errno::Errno::EINTR) => {}
            Err(_) => return false,
        }
        if start.elapsed() >= bound {
            return false;
        }
        std::thread::sleep(REAP_POLL_INTERVAL);
    }
}

#[cfg(not(unix))]
#[must_use]
pub fn reap_child_pid_bounded(_pid: u32, _bounds: ChildReapBounds) -> ChildReapEscalation {
    ChildReapEscalation::Voluntary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn interrupted_or_failed_wait_is_not_reaped_evidence() {
        assert!(!reap_poll_within(Duration::ZERO, || Err(
            nix::errno::Errno::EINTR
        )));
        assert!(!reap_poll_within(Duration::ZERO, || Err(
            nix::errno::Errno::EINVAL
        )));
        assert!(reap_poll_within(Duration::ZERO, || Err(
            nix::errno::Errno::ECHILD
        )));
        let mut calls = 0;
        assert!(reap_poll_within(Duration::from_secs(1), || {
            calls += 1;
            if calls == 1 {
                Err(nix::errno::Errno::EINTR)
            } else {
                Err(nix::errno::Errno::ECHILD)
            }
        }));
        assert_eq!(calls, 2);
    }

    #[test]
    fn children_rusage_delta_peak_rss_is_single_reap_max_only() {
        let before = ProcessResourceUsage {
            peak_rss_bytes: Some(4096),
            ..ProcessResourceUsage::default()
        };
        let after = ProcessResourceUsage {
            peak_rss_bytes: Some(8192),
            ..ProcessResourceUsage::default()
        };
        assert_eq!(children_rusage_delta(before, after).peak_rss_bytes, None);

        let fresh = ProcessResourceUsage {
            peak_rss_bytes: Some(8192),
            ..ProcessResourceUsage::default()
        };
        assert_eq!(
            children_rusage_delta(ProcessResourceUsage::default(), fresh).peak_rss_bytes,
            Some(8192)
        );
    }
}
