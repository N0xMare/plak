//! Descriptor-relative private files, locks and durable publication.

use base64::Engine as _;
use std::ffi::OsString;
use std::fs::{File, Metadata};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::MetadataExt as _;
use std::path::Component;
use std::path::Path;
use zeroize::{Zeroize as _, Zeroizing};

use rustix::fs::{AtFlags, FlockOperation, Mode, OFlags};

use super::{
    MAX_LOCK_IDENTITY_RETRIES, MAX_RECORD_FILE_BYTES, PrivateCredentialStore, PrivateStoreError,
    StoreAuthority, decode_record, encode_record,
};
use crate::{CredentialRecord, CredentialStatus, RecordAge};

pub(super) type DirectoryHandle = File;

pub(super) fn open_private_directory(
    path: &Path,
) -> Result<(DirectoryHandle, StoreAuthority), PrivateStoreError> {
    let parent = path.parent().ok_or(PrivateStoreError::InvalidPath)?;
    let root = rustix::fs::open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| PrivateStoreError::UnsafeDirectory)?;
    let mut directory = File::from(root);
    for component in parent.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                let next = rustix::fs::openat(
                    &directory,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|_| PrivateStoreError::UnsafeDirectory)?;
                directory = File::from(next);
                if !directory
                    .metadata()
                    .map_err(|_| PrivateStoreError::UnsafeDirectory)?
                    .file_type()
                    .is_dir()
                {
                    return Err(PrivateStoreError::UnsafeDirectory);
                }
            }
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(PrivateStoreError::InvalidPath);
            }
        }
    }
    let metadata = directory
        .metadata()
        .map_err(|_| PrivateStoreError::UnsafeDirectory)?;
    validate_directory(&metadata)?;
    Ok((
        directory,
        StoreAuthority {
            device: metadata.dev(),
            inode: metadata.ino(),
        },
    ))
}

/// Exclusive writer lock: the record lock file plus the private directory.
///
/// Every plain mutation takes this. The directory half serializes sibling
/// records and excludes an in-flight refresh claim, which owns the
/// directory through its execution lock.
pub(super) fn lock(store: &PrivateCredentialStore) -> Result<StoreLock, PrivateStoreError> {
    lock_with_mode_observer(store, LockTarget::Record, LockMode::Exclusive, true, |_| {
        Ok(())
    })?
    .ok_or(PrivateStoreError::LockFailed)
}

/// Non-blocking form of [`lock`]: `None` when any holder contends.
pub(super) fn try_lock(
    store: &PrivateCredentialStore,
) -> Result<Option<StoreLock>, PrivateStoreError> {
    lock_with_mode_observer(
        store,
        LockTarget::Record,
        LockMode::TryExclusive,
        true,
        |_| Ok(()),
    )
}

/// Shared reader lock on the record lock file only.
///
/// Readers and Ready leases never take the directory lock, so a status
/// read is delayed only by a writer's brief record replacement, never by
/// a refresh exchange that is waiting on the provider.
pub(super) fn lock_shared(
    store: &PrivateCredentialStore,
    nonblocking: bool,
) -> Result<Option<StoreLock>, PrivateStoreError> {
    let mode = if nonblocking {
        LockMode::TryShared
    } else {
        LockMode::Shared
    };
    lock_with_mode_observer(store, LockTarget::Record, mode, false, |_| Ok(()))
}

/// Non-blocking exclusive refresh execution lock plus the private directory.
///
/// A refresh claim owns this from before its durable Unknown write until
/// completion, restoration, or drop. Holding the directory excludes every
/// plain writer and every sibling claim; not holding the record lock file
/// leaves shared readers and Ready leases free.
pub(super) fn try_lock_execution(
    store: &PrivateCredentialStore,
) -> Result<Option<StoreLock>, PrivateStoreError> {
    lock_with_mode_observer(
        store,
        LockTarget::Execution,
        LockMode::TryExclusive,
        true,
        |_| Ok(()),
    )
}

/// Probes the execution lock with a non-blocking shared `flock` that is
/// released before return. A missing lock file means no claim ever ran;
/// the probe opens only an existing file and creates nothing.
pub(super) fn execution_lock_held(
    store: &PrivateCredentialStore,
) -> Result<bool, PrivateStoreError> {
    verify_explicit_path_authority(store)?;
    let file = match rustix::fs::openat(
        &store.directory,
        LockTarget::Execution.name(store),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(descriptor) => File::from(descriptor),
        Err(rustix::io::Errno::NOENT) => return Ok(false),
        Err(_) => return Err(PrivateStoreError::UnsafeLock),
    };
    validate_file(
        &file.metadata().map_err(|_| PrivateStoreError::UnsafeLock)?,
        true,
    )?;
    match rustix::fs::flock(&file, FlockOperation::NonBlockingLockShared) {
        Ok(()) => Ok(false),
        Err(rustix::io::Errno::AGAIN) => Ok(true),
        Err(_) => Err(PrivateStoreError::LockFailed),
    }
}

/// Exclusive record lock file only, for writes made under an owned
/// execution lock.
///
/// The directory is already held by the claim, so this waits only for
/// shared readers and Ready leases to drain.
pub(super) fn lock_record_for_claim(
    store: &PrivateCredentialStore,
    nonblocking: bool,
) -> Result<Option<StoreLock>, PrivateStoreError> {
    let mode = if nonblocking {
        LockMode::TryExclusive
    } else {
        LockMode::Exclusive
    };
    lock_with_mode_observer(store, LockTarget::Record, mode, false, |_| Ok(()))
}

/// Acquires the private directory (when requested) and then the target
/// lock file, in that fixed order.
///
/// The order is the global lock order of the store: directory, then
/// execution lock, then record lock. A refresh claim holds the directory
/// and later takes the record lock for its writes; taking the directory
/// before the record lock here keeps plain writers from holding the
/// record lock while they wait for the claim's directory. Every early
/// return drops its `File`s, and closing a lock's only descriptor
/// releases its `flock`.
fn lock_with_mode_observer(
    store: &PrivateCredentialStore,
    target: LockTarget,
    mode: LockMode,
    lock_directory: bool,
    mut after_open: impl FnMut(usize) -> Result<(), PrivateStoreError>,
) -> Result<Option<StoreLock>, PrivateStoreError> {
    validate_directory(
        &store
            .directory
            .metadata()
            .map_err(|_| PrivateStoreError::UnsafeDirectory)?,
    )?;
    let mut last_error = PrivateStoreError::UnsafeLock;
    for attempt in 0..MAX_LOCK_IDENTITY_RETRIES {
        verify_explicit_path_authority(store)?;
        let directory_lock = if lock_directory {
            let (directory, current_authority) = open_private_directory(&store.path)?;
            if current_authority != store.authority {
                return Err(PrivateStoreError::UnsafeDirectory);
            }
            #[cfg(test)]
            run_test_before_flock_observer(attempt)?;
            match rustix::fs::flock(&directory, mode.operation()) {
                Ok(()) => {}
                Err(rustix::io::Errno::AGAIN) if mode.is_nonblocking() => {
                    return Ok(None);
                }
                Err(_) => return Err(PrivateStoreError::LockFailed),
            }
            verify_explicit_path_authority(store)?;
            Some(directory)
        } else {
            None
        };
        let (file, created) = open_lock_candidate(store, target)?;
        if created {
            rustix::fs::fchmod(&file, Mode::RUSR | Mode::WUSR)
                .map_err(|_| PrivateStoreError::Io)?;
            file.sync_all().map_err(|_| PrivateStoreError::Io)?;
            store
                .directory
                .sync_all()
                .map_err(|_| PrivateStoreError::Io)?;
        }
        validate_file(
            &file.metadata().map_err(|_| PrivateStoreError::UnsafeLock)?,
            true,
        )?;
        after_open(attempt)?;
        verify_explicit_path_authority(store)?;
        #[cfg(test)]
        if !lock_directory {
            run_test_before_flock_observer(attempt)?;
        }
        match rustix::fs::flock(&file, mode.operation()) {
            Ok(()) => {}
            Err(rustix::io::Errno::AGAIN) if mode.is_nonblocking() => {
                return Ok(None);
            }
            Err(_) => return Err(PrivateStoreError::LockFailed),
        }
        verify_explicit_path_authority(store)?;
        let Ok(locked_metadata) = file.metadata() else {
            last_error = PrivateStoreError::UnsafeLock;
            continue;
        };
        if let Err(error) = validate_file(&locked_metadata, true) {
            last_error = error;
            continue;
        }
        match open_current_lock(store, target) {
            Ok(current) => {
                let current_metadata = current
                    .metadata()
                    .map_err(|_| PrivateStoreError::UnsafeLock)?;
                if same_file_identity(&locked_metadata, &current_metadata) {
                    verify_explicit_path_authority(store)?;
                    return Ok(Some(StoreLock {
                        file,
                        directory: directory_lock,
                        target,
                    }));
                }
                last_error = PrivateStoreError::UnsafeLock;
            }
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

pub(super) fn verify_explicit_path_authority(
    store: &PrivateCredentialStore,
) -> Result<(), PrivateStoreError> {
    let (current_directory, current_authority) = open_private_directory(&store.path)?;
    if current_authority != store.authority {
        return Err(PrivateStoreError::UnsafeDirectory);
    }
    let retained_metadata = store
        .directory
        .metadata()
        .map_err(|_| PrivateStoreError::UnsafeDirectory)?;
    let current_metadata = current_directory
        .metadata()
        .map_err(|_| PrivateStoreError::UnsafeDirectory)?;
    if !same_file_identity(&retained_metadata, &current_metadata) {
        return Err(PrivateStoreError::UnsafeDirectory);
    }
    Ok(())
}

#[cfg(test)]
type TestLockObserver = Box<dyn FnMut(usize) -> Result<(), PrivateStoreError>>;

#[cfg(test)]
thread_local! {
    static TEST_BEFORE_FLOCK_OBSERVER: std::cell::RefCell<Option<TestLockObserver>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn run_test_before_flock_observer(attempt: usize) -> Result<(), PrivateStoreError> {
    TEST_BEFORE_FLOCK_OBSERVER.with(|observer| {
        if let Some(observer) = observer.borrow_mut().as_mut() {
            observer(attempt)?;
        }
        Ok(())
    })
}

#[cfg(test)]
pub(super) fn with_test_before_flock_observer<T>(
    observer: impl FnMut(usize) -> Result<(), PrivateStoreError> + 'static,
    operation: impl FnOnce() -> T,
) -> T {
    struct ClearObserver;

    impl Drop for ClearObserver {
        fn drop(&mut self) {
            TEST_BEFORE_FLOCK_OBSERVER.with(|observer| {
                observer.borrow_mut().take();
            });
        }
    }

    TEST_BEFORE_FLOCK_OBSERVER.with(|slot| {
        assert!(
            slot.borrow().is_none(),
            "test lock observer already installed"
        );
        slot.borrow_mut().replace(Box::new(observer));
    });
    let _clear = ClearObserver;
    operation()
}

fn open_lock_candidate(
    store: &PrivateCredentialStore,
    target: LockTarget,
) -> Result<(File, bool), PrivateStoreError> {
    match rustix::fs::openat(
        &store.directory,
        target.name(store),
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    ) {
        Ok(descriptor) => Ok((File::from(descriptor), true)),
        Err(rustix::io::Errno::EXIST) => Ok((open_current_lock(store, target)?, false)),
        Err(_) => Err(PrivateStoreError::UnsafeLock),
    }
}

fn open_current_lock(
    store: &PrivateCredentialStore,
    target: LockTarget,
) -> Result<File, PrivateStoreError> {
    let descriptor = rustix::fs::openat(
        &store.directory,
        target.name(store),
        OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|_| PrivateStoreError::UnsafeLock)?;
    let file = File::from(descriptor);
    validate_file(
        &file.metadata().map_err(|_| PrivateStoreError::UnsafeLock)?,
        true,
    )?;
    Ok(file)
}

fn same_file_identity(first: &Metadata, second: &Metadata) -> bool {
    first.dev() == second.dev() && first.ino() == second.ino()
}

pub(super) fn verify_lock_authority(
    store: &PrivateCredentialStore,
    lock: &StoreLock,
) -> Result<(), PrivateStoreError> {
    verify_explicit_path_authority(store)?;
    if let Some(directory) = &lock.directory {
        let retained_directory = store
            .directory
            .metadata()
            .map_err(|_| PrivateStoreError::UnsafeDirectory)?;
        let locked_directory = directory
            .metadata()
            .map_err(|_| PrivateStoreError::UnsafeDirectory)?;
        if !same_file_identity(&retained_directory, &locked_directory) {
            return Err(PrivateStoreError::UnsafeDirectory);
        }
    }
    let locked_file = lock
        .file
        .metadata()
        .map_err(|_| PrivateStoreError::UnsafeLock)?;
    validate_file(&locked_file, true)?;
    let current_file = open_current_lock(store, lock.target)?;
    let current_metadata = current_file
        .metadata()
        .map_err(|_| PrivateStoreError::UnsafeLock)?;
    if !same_file_identity(&locked_file, &current_metadata) {
        return Err(PrivateStoreError::UnsafeLock);
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn lock_with_test_observer(
    store: &PrivateCredentialStore,
    observer: impl FnMut(usize) -> Result<(), PrivateStoreError>,
) -> Result<StoreLock, PrivateStoreError> {
    lock_with_mode_observer(
        store,
        LockTarget::Record,
        LockMode::Exclusive,
        true,
        observer,
    )?
    .ok_or(PrivateStoreError::LockFailed)
}

/// Which private lock file a [`StoreLock`] holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LockTarget {
    /// `.<record>.lock`: writers exclusive, readers and leases shared.
    Record,
    /// `.<record>.refresh.lock`: owned exclusively by one refresh claim.
    Execution,
}

impl LockTarget {
    fn name(self, store: &PrivateCredentialStore) -> &OsString {
        match self {
            Self::Record => &store.lock_name,
            Self::Execution => &store.execution_lock_name,
        }
    }
}

#[cfg(test)]
#[path = "unix/tests.rs"]
mod tests;

#[derive(Clone, Copy, Eq, PartialEq)]
enum LockMode {
    Exclusive,
    Shared,
    TryShared,
    TryExclusive,
}

impl LockMode {
    const fn is_nonblocking(self) -> bool {
        match self {
            Self::TryShared | Self::TryExclusive => true,
            Self::Exclusive | Self::Shared => false,
        }
    }

    const fn operation(self) -> FlockOperation {
        match self {
            Self::Exclusive => FlockOperation::LockExclusive,
            Self::Shared => FlockOperation::LockShared,
            Self::TryShared => FlockOperation::NonBlockingLockShared,
            Self::TryExclusive => FlockOperation::NonBlockingLockExclusive,
        }
    }
}

pub(super) fn load_record(
    store: &PrivateCredentialStore,
) -> Result<Option<CredentialRecord>, PrivateStoreError> {
    let record = load_record_for_local_retirement(store)?;
    match record.as_ref().map(|value| {
        (
            value.profile_id().age_against(&store.profile_id),
            value.status(),
        )
    }) {
        None | Some((RecordAge::Current, _)) => Ok(record),
        // A newer build's tombstone carries no login to lose.
        Some((RecordAge::Older, _) | (RecordAge::Newer, CredentialStatus::Revoked)) => {
            Err(PrivateStoreError::ForeignProfile)
        }
        Some((RecordAge::Newer, _)) => Err(PrivateStoreError::WrittenByNewerBuild),
    }
}

pub(super) fn load_record_for_local_retirement(
    store: &PrivateCredentialStore,
) -> Result<Option<CredentialRecord>, PrivateStoreError> {
    let descriptor = match rustix::fs::openat(
        &store.directory,
        &store.record_name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(_) => return Err(PrivateStoreError::UnsafeRecord),
    };
    let file = File::from(descriptor);
    let before = file
        .metadata()
        .map_err(|_| PrivateStoreError::UnsafeRecord)?;
    validate_file(&before, false)?;
    if before.len() > MAX_RECORD_FILE_BYTES {
        return Err(PrivateStoreError::RecordTooLarge);
    }
    let capacity = usize::try_from(before.len()).map_err(|_| PrivateStoreError::RecordTooLarge)?;
    let mut bytes = Zeroizing::new(Vec::with_capacity(capacity));
    (&file)
        .take(MAX_RECORD_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| PrivateStoreError::Io)?;
    if u64::try_from(bytes.len()).map_or(true, |length| length > MAX_RECORD_FILE_BYTES) {
        return Err(PrivateStoreError::RecordTooLarge);
    }
    let after = file
        .metadata()
        .map_err(|_| PrivateStoreError::UnsafeRecord)?;
    validate_file(&after, false)?;
    if !same_file_state(&before, &after) || u64::try_from(bytes.len()).ok() != Some(after.len()) {
        return Err(PrivateStoreError::UnsafeRecord);
    }
    let record = decode_record(store, &bytes)?;
    Ok(Some(record))
}

/// Durably removes the record; local retirement of a record whose
/// envelope this build cannot decode.
pub(super) fn remove_record(store: &PrivateCredentialStore) -> Result<(), PrivateStoreError> {
    rustix::fs::unlinkat(&store.directory, &store.record_name, AtFlags::empty())
        .map_err(|_| PrivateStoreError::Io)?;
    store
        .directory
        .sync_all()
        .map_err(|_| PrivateStoreError::Io)
}

pub(super) fn write_record(
    store: &PrivateCredentialStore,
    record: &CredentialRecord,
) -> Result<(), PrivateStoreError> {
    let bytes = encode_record(store, record)?;
    let temporary_name = temporary_name(&store.record_name)?;
    let descriptor = rustix::fs::openat(
        &store.directory,
        &temporary_name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|_| PrivateStoreError::Io)?;
    let mut temporary = File::from(descriptor);
    let result = (|| {
        rustix::fs::fchmod(&temporary, Mode::RUSR | Mode::WUSR)
            .map_err(|_| PrivateStoreError::Io)?;
        validate_file(
            &temporary
                .metadata()
                .map_err(|_| PrivateStoreError::UnsafeRecord)?,
            false,
        )?;
        temporary
            .write_all(&bytes)
            .map_err(|_| PrivateStoreError::Io)?;
        temporary.flush().map_err(|_| PrivateStoreError::Io)?;
        temporary.sync_all().map_err(|_| PrivateStoreError::Io)?;
        rustix::fs::renameat(
            &store.directory,
            &temporary_name,
            &store.directory,
            &store.record_name,
        )
        .map_err(|_| PrivateStoreError::Io)?;
        store
            .directory
            .sync_all()
            .map_err(|_| PrivateStoreError::Io)?;
        validate_installed_record(store)
    })();
    if result.is_err() {
        let _ = rustix::fs::unlinkat(&store.directory, &temporary_name, AtFlags::empty());
    }
    result
}

fn validate_installed_record(store: &PrivateCredentialStore) -> Result<(), PrivateStoreError> {
    let descriptor = rustix::fs::openat(
        &store.directory,
        &store.record_name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|_| PrivateStoreError::UnsafeRecord)?;
    let file = File::from(descriptor);
    validate_file(
        &file
            .metadata()
            .map_err(|_| PrivateStoreError::UnsafeRecord)?,
        false,
    )
}

fn validate_directory(metadata: &Metadata) -> Result<(), PrivateStoreError> {
    if !metadata.file_type().is_dir() {
        return Err(PrivateStoreError::UnsafeDirectory);
    }
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(PrivateStoreError::UnsafeOwnership);
    }
    if metadata.mode() & 0o7777 != 0o700 {
        return Err(PrivateStoreError::UnsafePermissions);
    }
    Ok(())
}

fn validate_file(metadata: &Metadata, lock: bool) -> Result<(), PrivateStoreError> {
    let unsafe_kind = if lock {
        PrivateStoreError::UnsafeLock
    } else {
        PrivateStoreError::UnsafeRecord
    };
    if !metadata.file_type().is_file() || metadata.nlink() != 1 {
        return Err(unsafe_kind);
    }
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(PrivateStoreError::UnsafeOwnership);
    }
    if metadata.mode() & 0o7777 != 0o600 {
        return Err(PrivateStoreError::UnsafePermissions);
    }
    Ok(())
}

fn same_file_state(before: &Metadata, after: &Metadata) -> bool {
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.len() == after.len()
        && before.uid() == after.uid()
        && before.mode() == after.mode()
        && before.nlink() == after.nlink()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
}

fn temporary_name(record_name: &std::ffi::OsStr) -> Result<OsString, PrivateStoreError> {
    let mut random = [0_u8; 18];
    getrandom::fill(&mut random).map_err(|_| PrivateStoreError::RandomnessUnavailable)?;
    let suffix = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random);
    random.zeroize();
    let record = record_name.to_str().ok_or(PrivateStoreError::InvalidPath)?;
    Ok(OsString::from(format!(".{record}.tmp.{suffix}")))
}

pub(super) struct StoreLock {
    file: File,
    directory: Option<File>,
    target: LockTarget,
}

impl StoreLock {
    /// Device/inode identity of the held lock file.
    pub(super) fn file_identity(&self) -> Result<StoreAuthority, PrivateStoreError> {
        let metadata = self
            .file
            .metadata()
            .map_err(|_| PrivateStoreError::UnsafeLock)?;
        Ok(StoreAuthority {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self.file, FlockOperation::Unlock);
        if let Some(directory) = &self.directory {
            let _ = rustix::fs::flock(directory, FlockOperation::Unlock);
        }
    }
}
