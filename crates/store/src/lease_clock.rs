//! Construction-bound continuous lease time, independent of observation UTC.

use plakous_agent::internal::{LeaseClock, LeaseDomain, LeaseInstant, RunId, SessionStoreError};
use std::str::FromStr;

pub(crate) struct SystemLeaseClock {
    domain: LeaseDomain,
    #[cfg(target_os = "linux")]
    _namespace: Option<std::fs::File>,
}

impl SystemLeaseClock {
    pub(crate) fn new() -> Result<Self, SessionStoreError> {
        #[cfg(target_os = "linux")]
        {
            let boot = read_boot_id()?;
            let (scope, namespace) = linux_namespace()?;
            let clock = Self {
                domain: LeaseDomain::new(boot, scope),
                _namespace: namespace,
            };
            clock.now()?;
            Ok(clock)
        }
        #[cfg(target_os = "macos")]
        {
            use sysctl::Sysctl;
            let boot = sysctl::Ctl::new("kern.bootsessionuuid")
                .and_then(|ctl| ctl.value_string())
                .map_err(|_| unavailable())?;
            let mut scope = [0; 32];
            scope[0] = 3;
            let clock = Self {
                domain: LeaseDomain::new(parse_boot_id(&boot)?, scope),
            };
            clock.now()?;
            Ok(clock)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        Err(unavailable())
    }
}

impl LeaseClock for SystemLeaseClock {
    fn now(&self) -> Result<LeaseInstant, SessionStoreError> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            #[cfg(target_os = "linux")]
            let id = nix::time::ClockId::CLOCK_BOOTTIME;
            #[cfg(target_os = "macos")]
            let id = nix::time::ClockId::from_raw(nix::libc::CLOCK_MONOTONIC_RAW);
            let time = nix::time::clock_gettime(id).map_err(|_| unavailable())?;
            let seconds = u64::try_from(time.tv_sec()).map_err(|_| unavailable())?;
            let nanos = u32::try_from(time.tv_nsec()).map_err(|_| unavailable())?;
            LeaseInstant::new(self.domain, seconds, nanos)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        Err(unavailable())
    }
}

fn unavailable() -> SessionStoreError {
    SessionStoreError::BackendUnavailable
}

fn parse_boot_id(value: &str) -> Result<[u8; 16], SessionStoreError> {
    let id = RunId::from_str(value.trim()).map_err(|_| unavailable())?;
    let bytes = *id.as_uuid().as_bytes();
    if bytes == [0; 16] {
        return Err(unavailable());
    }
    Ok(bytes)
}

#[cfg(target_os = "linux")]
fn read_boot_id() -> Result<[u8; 16], SessionStoreError> {
    use std::io::Read;
    let file = std::fs::File::open("/proc/sys/kernel/random/boot_id").map_err(|_| unavailable())?;
    let mut text = String::new();
    file.take(128)
        .read_to_string(&mut text)
        .map_err(|_| unavailable())?;
    parse_boot_id(&text)
}

#[cfg(target_os = "linux")]
fn linux_namespace() -> Result<([u8; 32], Option<std::fs::File>), SessionStoreError> {
    use rustix::fs::{Dir, Mode, OFlags, PROC_SUPER_MAGIC, fstatfs, openat};
    use std::os::unix::fs::MetadataExt;
    let directory = std::fs::File::open(format!("/proc/self/task/{}/ns", nix::unistd::gettid()))
        .map_err(|_| unavailable())?;
    if fstatfs(&directory).map_err(|_| unavailable())?.f_type != PROC_SUPER_MAGIC {
        return Err(unavailable());
    }
    let names = Dir::read_from(&directory).map_err(|_| unavailable())?;
    let time = has_time_namespace(names.map(|entry| {
        entry
            .map(|entry| entry.file_name().to_bytes().to_vec())
            .map_err(|_| unavailable())
    }))?;
    let mut scope = [0; 32];
    if !time {
        // A complete authentic procfs listing proves this kernel exposes no
        // time namespaces. A failed lookup is never this proof.
        scope[0] = 1;
        return Ok((scope, None));
    }
    let fd = openat(
        &directory,
        "time",
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| unavailable())?;
    let file = std::fs::File::from(fd);
    let metadata = file.metadata().map_err(|_| unavailable())?;
    scope[0] = 2;
    scope[1..9].copy_from_slice(&metadata.dev().to_be_bytes());
    scope[9..17].copy_from_slice(&metadata.ino().to_be_bytes());
    // Retaining the descriptor prevents namespace identity recycling while
    // this clock can still authorize a start.
    Ok((scope, Some(file)))
}

#[cfg(target_os = "linux")]
fn has_time_namespace(
    entries: impl Iterator<Item = Result<Vec<u8>, SessionStoreError>>,
) -> Result<bool, SessionStoreError> {
    let mut time = false;
    let mut children = false;
    let mut mount = false;
    for (index, entry) in entries.enumerate() {
        if index >= 64 {
            return Err(unavailable());
        }
        match entry?.as_slice() {
            b"time" => time = true,
            b"time_for_children" => children = true,
            b"mnt" => mount = true,
            _ => {}
        }
    }
    if !mount || time != children {
        return Err(unavailable());
    }
    Ok(time)
}

#[cfg(test)]
mod tests {
    use super::{SystemLeaseClock, parse_boot_id};
    use plakous_agent::internal::{LeaseClock, SessionStoreError};

    #[test]
    fn independently_constructed_clocks_share_authority() -> Result<(), SessionStoreError> {
        let first = SystemLeaseClock::new()?;
        let second = SystemLeaseClock::new()?;
        let before = first.now()?;
        let after = second.now()?;
        assert_eq!(before.domain(), after.domain());
        assert_ne!(before.same_domain_cmp(after)?, std::cmp::Ordering::Greater);
        Ok(())
    }

    #[test]
    fn boot_identity_refuses_missing_malformed_and_nil_values() {
        for value in ["", "not-a-uuid", "00000000-0000-0000-0000-000000000000"] {
            assert!(parse_boot_id(value).is_err());
        }
        assert!(parse_boot_id("de305d54-75b4-431b-adb2-eb6b9e546014\n").is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn namespace_absence_requires_complete_consistent_enumeration() {
        use super::has_time_namespace;
        let entries = |names: &[&str]| {
            names
                .iter()
                .map(|name| Ok(name.as_bytes().to_vec()))
                .collect::<Vec<_>>()
                .into_iter()
        };
        assert_eq!(has_time_namespace(entries(&["mnt", "pid"])), Ok(false));
        assert_eq!(
            has_time_namespace(entries(&["mnt", "time", "time_for_children"])),
            Ok(true)
        );
        for names in [
            vec!["time", "time_for_children"],
            vec!["mnt", "time"],
            vec!["mnt", "time_for_children"],
        ] {
            assert!(has_time_namespace(entries(&names)).is_err());
        }
        let partial =
            entries(&["mnt"]).chain(std::iter::once(Err(SessionStoreError::BackendUnavailable)));
        assert!(has_time_namespace(partial).is_err());
        assert!(has_time_namespace(entries(&vec!["mnt"; 65])).is_err());
    }
}
