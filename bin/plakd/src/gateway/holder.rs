//! The credential-store holder lock.
//!
//! Only the service holds the lock. Once listening, it advertises the socket
//! so a Unix host can prove that the listener holds its exact state store.

use std::fs::File;
use std::io::{self, Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation, Mode, OFlags, flock, open};

use super::io_file::{ensure_private_dir, private_file};

/// One held lock. Dropping it releases the kernel lock.
pub(crate) struct HolderLock {
    file: Option<File>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HolderMode {
    Exclusive,
    #[cfg(test)]
    Shared,
}

impl HolderLock {
    /// A concurrent startup probe may briefly hold a shared lock on a vacant
    /// store. Give it time to release without waiting for an active gateway.
    pub(super) fn acquire_service(path: &Path) -> io::Result<Self> {
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_millis(150);
        loop {
            match Self::try_acquire(path, HolderMode::Exclusive) {
                Err(error) if is_busy(&error) && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                result => return result,
            }
        }
    }

    /// Takes the lock without waiting. `Busy` means the other mode holds it.
    pub(crate) fn try_acquire(path: &Path, mode: HolderMode) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            ensure_private_dir(parent)?;
        }
        let file = private_file(path)?;
        let operation = match mode {
            HolderMode::Exclusive => FlockOperation::NonBlockingLockExclusive,
            #[cfg(test)]
            HolderMode::Shared => FlockOperation::NonBlockingLockShared,
        };
        flock(&file, operation)?;
        if mode == HolderMode::Exclusive {
            // A previous service may have exited without clearing its path.
            file.set_len(0)?;
        }
        Ok(Self { file: Some(file) })
    }

    #[cfg(test)]
    pub(crate) fn vacant() -> Self {
        Self { file: None }
    }

    /// Records the service socket. Only the exclusive holder writes it.
    pub(crate) fn advertise(&self, socket: &Path) -> io::Result<()> {
        let Some(file) = self.file.as_ref() else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no holder file"));
        };
        let text = socket
            .to_str()
            .filter(|text| !text.contains('\n') && !text.is_empty())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket path"))?;
        let mut file = file.try_clone()?;
        file.set_len(0)?;
        file.seek(io::SeekFrom::Start(0))?;
        file.write_all(text.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        Ok(())
    }
}

/// The socket a live exclusive holder advertised, if this process cannot take
/// the shared lock.
pub(crate) fn advertised_socket(path: &Path) -> io::Result<Option<PathBuf>> {
    let file = match open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(file) => File::from(file),
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid gateway holder file",
        ));
    }
    match flock(&file, FlockOperation::NonBlockingLockShared) {
        Ok(()) => return Ok(None),
        Err(error) if is_busy(&error.into()) => (),
        Err(error) => return Err(error.into()),
    }
    let mut text = String::new();
    file.take(4096).read_to_string(&mut text)?;
    if text.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "the gateway holder has not advertised a listening socket",
        ));
    }
    let line = text.trim();
    let path = PathBuf::from(line);
    if line.is_empty() || !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the credential holder advertises an invalid socket",
        ));
    }
    Ok(Some(path))
}

pub(crate) fn lock_path(state_root: &Path) -> PathBuf {
    state_root.join("plak/subscriptions/.custody-holder.lock")
}

/// rustix reports a contended non-blocking flock as `WouldBlock`.
pub(crate) fn is_busy(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock
        || error.raw_os_error() == Some(rustix::io::Errno::WOULDBLOCK.raw_os_error())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn exclusive_and_shared_do_not_overlap() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("holder.lock");
        let exclusive = HolderLock::try_acquire(&path, HolderMode::Exclusive).unwrap();
        let socket = dir.path().join("gateway.sock");
        exclusive.advertise(&socket).unwrap();
        assert!(
            HolderLock::try_acquire(&path, HolderMode::Shared).is_err(),
            "shared lock must fail while the service holds exclusive"
        );
        let found = advertised_socket(&path).unwrap().unwrap();
        assert_eq!(found, socket);
        drop(exclusive);
        let shared = HolderLock::try_acquire(&path, HolderMode::Shared).unwrap();
        assert!(HolderLock::try_acquire(&path, HolderMode::Exclusive).is_err());
        drop(shared);
        assert!(advertised_socket(&path).unwrap().is_none());
        let absent = dir.path().join("absent");
        assert_eq!(
            advertised_socket(&absent.join("holder.lock")).unwrap(),
            None
        );
        assert!(
            !absent.exists(),
            "a missing store must not be created to take the shared lock"
        );
    }
}
