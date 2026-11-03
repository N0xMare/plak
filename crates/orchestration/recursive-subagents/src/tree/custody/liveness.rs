//! Custody's own holder-gone proof (spec 03 §14).
//!
//! A custody holder can hold a dormant tree with no runtime lease, so a
//! lease's holder record proves nothing about it. Instead, before its first
//! take in a store, each host process takes one exclusive advisory lock on a
//! file named after its instance, `<instance>.lock`, in the store's
//! `custody-hosts` directory, and holds it for the rest of its life. The
//! kernel releases the lock when the process ends, however it ends. A
//! holder on this machine whose lock this process can take is therefore
//! proven gone. A missing lock file proves nothing: it may have been removed
//! under a live holder. There is no time-based expiry, and a holder on
//! another machine is never proven gone here.
//!
//! Taking the own lock sweeps the free locks of instances that no held
//! custody record names, which bounds the directory without removing the
//! one proof a crashed holder's record still needs. A crashed holder's free
//! lock stays until its record is taken over or released.
//!
//! Every descriptor is opened close-on-exec, so a child the host spawns does
//! not keep its lock past `exec`. A lock is trusted only while the path still
//! names the file that was locked (the inode check), so a lock file that was
//! swept and replaced between the open and the lock is never mistaken for
//! the one a holder holds.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

use super::{CustodyHost, CustodyLiveness, HostInstance, TreeCustody, sealed};

/// Attempts at this process's own lock before a take gives up for now.
const LOCK_ATTEMPTS: u32 = 3;

/// This process's locks, one per custody directory, held until it exits.
static LOCKS: OnceLock<Mutex<HashMap<PathBuf, File>>> = OnceLock::new();

fn identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

fn lock_path(directory: &Path, instance: HostInstance) -> PathBuf {
    directory.join(format!("{instance}.lock"))
}

/// The private directory the locks live in: created 0700, refused when the
/// path is a link or anything but a directory.
fn private_directory(directory: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    let metadata = std::fs::symlink_metadata(directory)?;
    if !metadata.is_dir() {
        return Err(io::Error::other(
            "the custody host directory is not a directory",
        ));
    }
    Ok(())
}

/// Takes this process's lock in `directory` once, before its first custody
/// take in `store`, and holds it for the process's life; then sweeps the free
/// locks that no held record of `store` names.
///
/// # Errors
///
/// The directory or the lock cannot be made or taken.
pub(super) fn hold_own_lock(
    directory: &Path,
    instance: HostInstance,
    store: &dyn TreeCustody,
) -> io::Result<()> {
    let mut locks = LOCKS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let path = lock_path(directory, instance);
    if locks
        .get(directory)
        .is_some_and(|held| still_named(held, &path))
    {
        return Ok(());
    }
    private_directory(directory)?;
    let mut last = io::Error::other("custody host lock was not taken");
    for _ in 0..LOCK_ATTEMPTS {
        match take_own(&path) {
            Ok(file) => {
                locks.insert(directory.to_path_buf(), file);
                drop(locks);
                sweep(directory, instance, store);
                return Ok(());
            }
            Err(error) => last = error,
        }
    }
    Err(last)
}

/// Creates (or reopens) this instance's lock file and locks it, keeping it
/// only while the path still names the locked file: a concurrent sweep that
/// removed it between the create and the lock leaves the next attempt to
/// create it again.
fn take_own(path: &Path) -> io::Result<File> {
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            OpenOptions::new().read(true).write(true).open(path)?
        }
        Err(error) => return Err(error),
    };
    file.lock()?;
    if still_named(&file, path) {
        Ok(file)
    } else {
        Err(io::Error::other(
            "the custody host lock was replaced while it was taken",
        ))
    }
}

/// Whether `path` still names exactly the regular file `file` has open.
fn still_named(file: &File, path: &Path) -> bool {
    match (file.metadata(), std::fs::symlink_metadata(path)) {
        (Ok(open), Ok(named)) => named.is_file() && identity(&open) == identity(&named),
        _ => false,
    }
}

/// Removes the free locks of other instances that no held record of `store`
/// names, each only while it is still the file that was locked. A lock
/// another process holds is left alone.
///
/// The free locks are taken before the held records are read: an instance
/// whose lock is free has ended, so the records naming it can only be taken
/// over or released after that read, never newly claimed.
fn sweep(directory: &Path, own: HostInstance, store: &dyn TreeCustody) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let own = format!("{own}.lock");
    let mut free = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(instance) = name.strip_suffix(".lock") else {
            continue;
        };
        if name == own {
            continue;
        }
        let Ok(instance) = HostInstance::parse(instance) else {
            continue;
        };
        let path = entry.path();
        if let Some(file) = free_lock(&path) {
            free.push((instance, path, file));
        }
    }
    if free.is_empty() {
        return;
    }
    let Ok(named) = store.held_custody_instances() else {
        return;
    };
    for (instance, path, file) in free {
        if !named.contains(&instance) {
            let _ = std::fs::remove_file(&path);
        }
        drop(file);
    }
}

/// The lock file at `path`, locked by this call, when no process holds it
/// and the path still names it; `None` otherwise.
///
/// The one identity check that matters is after the lock: whatever the path
/// named at the open, only a lock on the file it names now is a proof. The
/// type check before the open keeps it from blocking on a FIFO.
fn free_lock(path: &Path) -> Option<File> {
    if !std::fs::symlink_metadata(path).ok()?.is_file() {
        return None;
    }
    let file = File::open(path).ok()?;
    #[cfg(test)]
    tests::before_lock(path);
    file.try_lock().ok()?;
    still_named(&file, path).then_some(file)
}

/// Whether the lock of `instance` in `directory` is proven held by a live
/// process: the file is there and cannot be locked. A missing file is not
/// proof either way.
pub(super) fn lock_held(directory: &Path, instance: HostInstance) -> bool {
    let path = lock_path(directory, instance);
    std::fs::symlink_metadata(&path).is_ok_and(|named| named.is_file())
        && File::open(&path)
            .is_ok_and(|file| matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock)))
}

/// Liveness over one custody directory, as `own` sees it.
pub(super) struct DirectoryLiveness<'a> {
    pub(super) directory: &'a Path,
    pub(super) own: &'a CustodyHost,
}

impl sealed::Sealed for DirectoryLiveness<'_> {}

impl CustodyLiveness for DirectoryLiveness<'_> {
    fn holder_gone(&self, holder: &CustodyHost) -> bool {
        // Another machine's process is never proven gone from here, and this
        // process is never gone to itself.
        if holder.host_name != self.own.host_name || holder.instance == self.own.instance {
            return false;
        }
        // A holder locked its file before its take and holds it while it
        // lives. A missing file may have been removed under it, so only a
        // lock taken here proves it gone.
        free_lock(&lock_path(self.directory, holder.instance)).is_some()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
