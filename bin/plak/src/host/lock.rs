//! `host.lock`: one `plak host` per home and state key. The lock is taken
//! before anything else and held for the process's life; the kernel
//! releases it however the process ends. The file names its holder, so a
//! second host can say who serves; the lock, not the text, proves it.

use std::fs::{File, OpenOptions};
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
        use std::os::unix::fs::OpenOptionsExt as _;
        let failed = |error: std::io::Error| {
            CliFailure::new(format!("{} could not be locked: {error}", path.display()))
        };
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(failed)?;
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
                let _ = file.read_to_string(&mut holder);
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
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

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
