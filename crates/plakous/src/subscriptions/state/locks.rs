use super::{
    File, FlockOperation, Mode, NativeSubscriptionError, OFlags, OsString, Path, PathBuf,
    PrivateFileIdentity, open_private_parent, private_file_identity, validate_private_directory,
    validate_private_file, validate_private_file_metadata,
};

#[cfg(unix)]
pub(in crate::subscriptions) struct PrivateFilesystemLock {
    file: File,
    active_path: PathBuf,
    parent: File,
    parent_identity: PrivateFileIdentity,
    name: OsString,
    lock_identity: PrivateFileIdentity,
}

#[cfg(unix)]
impl Drop for PrivateFilesystemLock {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self.file, FlockOperation::Unlock);
        let _ = rustix::fs::flock(&self.parent, FlockOperation::Unlock);
    }
}

#[cfg(unix)]
impl PrivateFilesystemLock {
    pub(in crate::subscriptions) fn verify(&self) -> Result<(), NativeSubscriptionError> {
        validate_private_directory(&self.parent)?;
        let retained_parent = self
            .parent
            .metadata()
            .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
        if private_file_identity(&retained_parent) != self.parent_identity {
            return Err(NativeSubscriptionError::UnsafeStorage);
        }

        let retained_lock = self
            .file
            .metadata()
            .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
        validate_private_file_metadata(&retained_lock)?;
        if private_file_identity(&retained_lock) != self.lock_identity {
            return Err(NativeSubscriptionError::UnsafeStorage);
        }

        let (active_parent, active_name) = open_private_parent(&self.active_path)?;
        let active_parent_metadata = active_parent
            .metadata()
            .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
        if active_name != self.name
            || private_file_identity(&active_parent_metadata) != self.parent_identity
        {
            return Err(NativeSubscriptionError::UnsafeStorage);
        }
        let active_lock = rustix::fs::openat(
            &active_parent,
            &active_name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
        let active_lock_metadata = active_lock
            .metadata()
            .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
        validate_private_file_metadata(&active_lock_metadata)?;
        if private_file_identity(&active_lock_metadata) != self.lock_identity {
            return Err(NativeSubscriptionError::UnsafeStorage);
        }
        Ok(())
    }
}

#[cfg(unix)]
pub(super) fn acquire_private_filesystem_lock(
    path: &Path,
) -> Result<PrivateFilesystemLock, NativeSubscriptionError> {
    acquire_private_filesystem_lock_inner(path, false)?.ok_or(NativeSubscriptionError::Internal)
}

#[cfg(unix)]
pub(super) fn try_acquire_private_filesystem_lock(
    path: &Path,
) -> Result<Option<PrivateFilesystemLock>, NativeSubscriptionError> {
    acquire_private_filesystem_lock_inner(path, true)
}

#[cfg(unix)]
pub(super) fn acquire_private_filesystem_lock_inner(
    path: &Path,
    nonblocking: bool,
) -> Result<Option<PrivateFilesystemLock>, NativeSubscriptionError> {
    if !path.is_absolute() {
        return Err(NativeSubscriptionError::UnsafeStorage);
    }
    let (directory, name) = open_private_parent(path)?;
    let operation = if nonblocking {
        FlockOperation::NonBlockingLockExclusive
    } else {
        FlockOperation::LockExclusive
    };
    if let Err(error) = rustix::fs::flock(&directory, operation) {
        if nonblocking && error == rustix::io::Errno::WOULDBLOCK {
            return Ok(None);
        }
        return Err(NativeSubscriptionError::UnsafeStorage);
    }
    let parent_identity = private_file_identity(
        &directory
            .metadata()
            .map_err(|_| NativeSubscriptionError::UnsafeStorage)?,
    );
    let descriptor = rustix::fs::openat(
        &directory,
        &name,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
    let file = File::from(descriptor);
    rustix::fs::fchmod(&file, Mode::from_raw_mode(0o600))
        .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
    validate_private_file(&file)?;
    if let Err(error) = rustix::fs::flock(&file, operation) {
        if nonblocking && error == rustix::io::Errno::WOULDBLOCK {
            return Ok(None);
        }
        return Err(NativeSubscriptionError::UnsafeStorage);
    }
    let locked_metadata = file
        .metadata()
        .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
    validate_private_file_metadata(&locked_metadata)?;
    let current = rustix::fs::openat(
        &directory,
        &name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
    let current_metadata = current
        .metadata()
        .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
    validate_private_file_metadata(&current_metadata)?;
    let lock_identity = private_file_identity(&locked_metadata);
    if lock_identity != private_file_identity(&current_metadata) {
        let _ = rustix::fs::flock(&file, FlockOperation::Unlock);
        return Err(NativeSubscriptionError::UnsafeStorage);
    }
    let lock = PrivateFilesystemLock {
        file,
        active_path: path.to_path_buf(),
        parent: directory,
        parent_identity,
        name,
        lock_identity,
    };
    lock.verify()?;
    Ok(Some(lock))
}

#[cfg(not(unix))]
pub(in crate::subscriptions) struct PrivateFilesystemLock;

#[cfg(not(unix))]
impl PrivateFilesystemLock {
    pub(in crate::subscriptions) fn verify(&self) -> Result<(), NativeSubscriptionError> {
        Err(NativeSubscriptionError::UnsupportedPlatform)
    }
}

#[cfg(not(unix))]
pub(super) fn acquire_private_filesystem_lock(
    _path: &Path,
) -> Result<PrivateFilesystemLock, NativeSubscriptionError> {
    Err(NativeSubscriptionError::UnsupportedPlatform)
}

#[cfg(not(unix))]
pub(super) fn try_acquire_private_filesystem_lock(
    _path: &Path,
) -> Result<Option<PrivateFilesystemLock>, NativeSubscriptionError> {
    Err(NativeSubscriptionError::UnsupportedPlatform)
}
