//! Process resource sampling for the ready-path timing report.

use std::time::Duration;

use super::{ProcessResourceSnapshot, ProcessResources};

/// `ru_maxrss` is kilobytes on Linux and bytes on macOS.
#[cfg(target_os = "linux")]
pub(super) fn normalize_max_rss_bytes(raw: u64) -> u64 {
    raw.saturating_mul(1024)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn normalize_max_rss_bytes(raw: u64) -> u64 {
    raw
}

pub(super) fn timeval_duration(secs: u64, micros: u64) -> Duration {
    Duration::from_secs(secs).saturating_add(Duration::from_micros(micros))
}

/// Samples this process via `getrusage` (Unix) and, on Linux, `/proc/self`.
#[must_use]
pub fn sample_process_resources() -> ProcessResourceSnapshot {
    let mut snapshot = ProcessResourceSnapshot::default();
    #[cfg(unix)]
    {
        use nix::sys::resource::{UsageWho, getrusage};
        if let Ok(usage) = getrusage(UsageWho::RUSAGE_SELF) {
            let user = usage.user_time();
            let system = usage.system_time();
            snapshot.user_cpu = timeval_duration(
                user.tv_sec().try_into().unwrap_or(0),
                user.tv_usec().try_into().unwrap_or(0),
            );
            snapshot.system_cpu = timeval_duration(
                system.tv_sec().try_into().unwrap_or(0),
                system.tv_usec().try_into().unwrap_or(0),
            );
            let max_rss = usage.max_rss();
            if max_rss > 0 {
                snapshot.peak_rss_bytes =
                    Some(normalize_max_rss_bytes(u64::try_from(max_rss).unwrap_or(0)));
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            let mut rss_kb = 0_u64;
            let mut peak_rss_kb = 0_u64;
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
                    "VmHWM:" => peak_rss_kb = value,
                    "Threads:" => threads = value,
                    _ => {}
                }
            }
            if rss_kb > 0 {
                snapshot.rss_bytes = Some(rss_kb.saturating_mul(1024));
            }
            if peak_rss_kb > 0 {
                snapshot.peak_rss_bytes = Some(peak_rss_kb.saturating_mul(1024));
            }
            if threads > 0 {
                snapshot.threads = Some(threads);
            }
        }
        snapshot.fds = std::fs::read_dir("/proc/self/fd")
            .ok()
            .and_then(|entries| u64::try_from(entries.count()).ok());
    }
    snapshot
}

/// Samples cumulative resource usage for all reaped children (`RUSAGE_CHILDREN`).
#[must_use]
#[allow(dead_code, reason = "reserved for tool-shell reap wiring")]
pub(crate) fn sample_children_rusage_cumulative() -> ProcessResourceSnapshot {
    let mut snapshot = ProcessResourceSnapshot::default();
    #[cfg(unix)]
    {
        use nix::sys::resource::{UsageWho, getrusage};
        if let Ok(usage) = getrusage(UsageWho::RUSAGE_CHILDREN) {
            let user = usage.user_time();
            let system = usage.system_time();
            snapshot.user_cpu = timeval_duration(
                user.tv_sec().try_into().unwrap_or(0),
                user.tv_usec().try_into().unwrap_or(0),
            );
            snapshot.system_cpu = timeval_duration(
                system.tv_sec().try_into().unwrap_or(0),
                system.tv_usec().try_into().unwrap_or(0),
            );
            let max_rss = usage.max_rss();
            if max_rss > 0 {
                snapshot.peak_rss_bytes =
                    Some(normalize_max_rss_bytes(u64::try_from(max_rss).unwrap_or(0)));
            }
        }
    }
    snapshot
}

pub(super) fn process_resources() -> Option<ProcessResources> {
    #[cfg(target_os = "linux")]
    {
        let snapshot = sample_process_resources();
        Some(ProcessResources {
            rss_kb: snapshot.rss_bytes.unwrap_or(0) / 1024,
            peak_rss_kb: snapshot.peak_rss_bytes.unwrap_or(0) / 1024,
            threads: snapshot.threads.unwrap_or(0),
            fds: snapshot.fds.unwrap_or(0),
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;

    #[test]
    fn process_resources_reads_linux_status() {
        let Some(resources) = process_resources() else {
            return;
        };
        assert!(resources.rss_kb > 0, "VmRSS must be present: {resources:?}");
        assert!(resources.peak_rss_kb >= resources.rss_kb);
        assert!(resources.threads >= 1);
        assert!(resources.fds >= 3);
    }
}
