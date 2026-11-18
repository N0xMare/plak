//! The credential-store holder lock.
//!
//! In-process gateways take it shared. The service takes it exclusive, so the
//! two modes cannot overlap. While the service holds it, the file body is the
//! service socket path a local CLI discovers.

use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation, flock};

use super::io_file::{ensure_private_dir, private_file};

/// One held lock. Dropping it releases the kernel lock.
pub(crate) struct HolderLock {
    file: File,
    mode: HolderMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HolderMode {
    Exclusive,
    Shared,
}

impl HolderLock {
    /// Takes the lock without waiting. `Busy` means the other mode holds it.
    pub(crate) fn try_acquire(path: &Path, mode: HolderMode) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            ensure_private_dir(parent)?;
        }
        let file = private_file(path)?;
        let operation = match mode {
            HolderMode::Exclusive => FlockOperation::NonBlockingLockExclusive,
            HolderMode::Shared => FlockOperation::NonBlockingLockShared,
        };
        flock(&file, operation)?;
        Ok(Self { file, mode })
    }

    /// Records the service socket. Only the exclusive holder writes it.
    pub(crate) fn advertise(&self, socket: &Path) -> io::Result<()> {
        if self.mode != HolderMode::Exclusive {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "only the service holder advertises a socket",
            ));
        }
        let text = socket
            .to_str()
            .filter(|text| !text.contains('\n') && !text.is_empty())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket path"))?;
        let mut file = self.file.try_clone()?;
        file.set_len(0)?;
        file.write_all(text.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        Ok(())
    }
}

/// The socket a live exclusive holder advertised, if this process cannot take
/// the shared lock.
pub(crate) fn advertised_socket(path: &Path) -> io::Result<Option<PathBuf>> {
    match HolderLock::try_acquire(path, HolderMode::Shared) {
        Ok(_lock) => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            read_advertisement(path).map(Some)
        }
        Err(error) => Err(error),
    }
}

fn read_advertisement(path: &Path) -> io::Result<PathBuf> {
    let mut text = String::new();
    File::open(path)?.read_to_string(&mut text)?;
    let line = text.trim();
    let path = PathBuf::from(line);
    if line.is_empty() || !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the credential holder is in-process and advertises no socket",
        ));
    }
    Ok(path)
}

pub(crate) fn lock_path(state_root: &Path) -> PathBuf {
    state_root.join("plak/subscriptions/.custody-holder.lock")
}

/// Shared hold for an in-process account command or host. Fails while the
/// service holds the store, and the message names the config remedy.
pub(crate) fn acquire_shared(state_root: &Path) -> Result<HolderLock, String> {
    HolderLock::try_acquire(&lock_path(state_root), HolderMode::Shared).map_err(|error| {
        if is_busy(&error) {
            "the credential store is held by plakd gateway serve; set [gateway] address".to_owned()
        } else {
            error.to_string()
        }
    })
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
        let held = acquire_shared(dir.path()).unwrap();
        let blocked = HolderLock::try_acquire(&lock_path(dir.path()), HolderMode::Exclusive);
        assert!(
            blocked.is_err(),
            "a service must not overlap an in-process holder"
        );
        drop(held);
        assert!(HolderLock::try_acquire(&lock_path(dir.path()), HolderMode::Exclusive).is_ok());
        let exclusive =
            HolderLock::try_acquire(&lock_path(dir.path()), HolderMode::Exclusive).unwrap();
        match acquire_shared(dir.path()) {
            Err(message) => assert!(message.contains("[gateway] address"), "{message}"),
            Ok(_lock) => panic!("shared acquire must fail while the service holds the store"),
        }
        drop(exclusive);
    }
}
