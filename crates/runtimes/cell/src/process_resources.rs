//! Process resource sampling for sidecar parent clients.

use std::time::Duration;

/// Content-free resource snapshot.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
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

#[cfg(test)]
mod tests {
    use super::*;

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
