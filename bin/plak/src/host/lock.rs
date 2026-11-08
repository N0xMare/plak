//! Home-wide and per-state-key locks: one `plak host` per home. The locks
//! precede host startup and remain held through handoff; the kernel
//! releases it however the process ends. The file names its holder, so a
//! second host can say who serves; the lock, not the text, proves it.

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::Path;

use plakous_hosted::error::{CliFailure, CliResult};

/// The held lock; dropping it releases the home's state to the next host.
#[derive(Debug)]
pub(super) struct HostLock {
    file: File,
}

impl Drop for HostLock {
    fn drop(&mut self) {
        // Clear the diagnostic while still owning the kernel lock. Crashes
        // can still leave stale text, so discovery must confirm that lock.
        let _ = self.file.set_len(0);
    }
}

impl HostLock {
    /// Serializes all current-key starts and refuses any already-live state key.
    pub(super) fn take_home(
        paths: &crate::product_paths::PlakPaths,
        binary: &str,
    ) -> CliResult<(Self, Self)> {
        use crate::wire_transport::hosts;
        crate::commands::runner::private_directory(&paths.home().join("host"))?;
        let home = Self::take(&hosts::home_lock(paths), binary);
        let live = hosts::live(paths)?;
        if !live.is_empty() {
            return Err(hosts::conflict(&live));
        }
        let home = home?;
        let state = Self::take(&paths.host_lock(), binary)?;
        Ok((home, state))
    }

    /// Takes the lock at `path`, naming this process and `binary` as its
    /// holder.
    ///
    /// # Errors
    ///
    /// Another host holds it (the refusal names its pid and binary), or the
    /// file cannot be opened or written.
    pub(super) fn take(path: &Path, binary: &str) -> CliResult<Self> {
        Self::take_with_retry(path, binary, || {
            std::thread::sleep(std::time::Duration::from_millis(5));
        })
    }

    fn take_with_retry(path: &Path, binary: &str, mut pause: impl FnMut()) -> CliResult<Self> {
        use rustix::fs::{Mode, OFlags};
        use std::os::unix::fs::MetadataExt as _;
        let failed = |error: std::io::Error| {
            CliFailure::new(format!("{} could not be locked: {error}", path.display()))
        };
        let mut file = File::from(
            rustix::fs::open(
                path,
                OFlags::CREATE
                    | OFlags::RDWR
                    | OFlags::NOFOLLOW
                    | OFlags::NONBLOCK
                    | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(|error| failed(error.into()))?,
        );
        let metadata = file.metadata().map_err(failed)?;
        if !metadata.is_file()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o077 != 0
            || metadata.len() > 4096
        {
            return Err(CliFailure::new(
                "host lock is not this user's bounded private regular file",
            ));
        }
        let mut result = file.try_lock();
        for _ in 0..40 {
            if !matches!(result, Err(std::fs::TryLockError::WouldBlock)) {
                break;
            }
            pause();
            result = file.try_lock();
        }
        match result {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                let mut holder = String::new();
                (&mut file)
                    .take(4097)
                    .read_to_string(&mut holder)
                    .map_err(failed)?;
                if holder.len() > 4096 {
                    return Err(CliFailure::new(
                        "host lock diagnostic grew beyond its bound",
                    ));
                }
                return Err(CliFailure::new(held_by(&holder)));
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(failed(error)),
        }
        file.set_len(0).map_err(failed)?;
        writeln!(file, "{} {binary}", std::process::id()).map_err(failed)?;
        file.sync_all().map_err(failed)?;
        Ok(Self { file })
    }
}

/// The refusal a second host gets, from the holder's `<pid> <binary>` line.
fn held_by(holder: &str) -> String {
    let mut fields = holder.split_whitespace();
    match (fields.next(), fields.next()) {
        (Some(pid), Some(binary)) => format!(
            "plak host is already running for this home's state (pid {pid}, binary {binary})"
        ),
        _ => "plak host is already running for this home's state".to_owned(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use std::fs::OpenOptions;
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn a_linked_lock_refuses_without_modifying_its_target() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("not-a-lock");
        std::fs::write(&target, "leave this file intact").unwrap();
        let path = root.path().join("host.lock");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(HostLock::take(&path, "fixture-binary").is_err());
        assert_eq!(
            std::fs::read_to_string(target).unwrap(),
            "leave this file intact"
        );
    }

    /// A second host refuses while the first holds the lock, naming the
    /// holder; once the first is gone the next one takes it.
    #[test]
    fn a_second_host_refuses_naming_the_holder_until_the_first_is_gone() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("host.lock");
        let first = HostLock::take(&path, "0123456789ab").unwrap();
        let refused = HostLock::take(&path, "ba9876543210").unwrap_err();
        assert_eq!(
            refused.message(),
            format!(
                "plak host is already running for this home's state (pid {}, binary 0123456789ab)",
                std::process::id()
            )
        );
        drop(first);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        let _second = HostLock::take(&path, "ba9876543210").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{} ba9876543210\n", std::process::id())
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn a_holder_line_that_does_not_parse_still_refuses() {
        assert_eq!(
            held_by(""),
            "plak host is already running for this home's state"
        );
    }

    #[test]
    fn a_host_starting_during_a_discovery_probe_retries_both_contentions() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("host.lock");
        std::fs::write(&path, "1 stale-build\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let probe = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        probe.lock().unwrap();
        let mut pauses = 0;
        let _host = HostLock::take_with_retry(&path, "new-build", || {
            pauses += 1;
            if pauses == 30 {
                probe.unlock().unwrap();
            }
        })
        .unwrap();
        assert_eq!(
            pauses, 30,
            "a starting host must outwait a transient discovery probe"
        );
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            format!("{} new-build\n", std::process::id())
        );
        assert!(matches!(
            probe.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
    }
}
