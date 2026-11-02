//! Host-local proof that a branch lease's holder is gone.
//!
//! A lease is a time-bounded promise: the holder renews it while it lives, and
//! another process may take the branch once it stops. That is the right rule
//! for a holder this process cannot see — one on another machine, or one whose
//! identity was never recorded — because the only honest evidence is the clock.
//!
//! It is the wrong rule for the common case. When a `plak` dies mid-turn on the
//! machine you are sitting at, waiting out its lease means waiting to reopen a
//! session that nothing is using, and the operator reads that as broken.
//!
//! So each process that takes a branch also takes one exclusive advisory lock
//! on a file of its own, `<identity>.lock`, in a private directory beside the
//! database, and holds it while it has the store open. Every grant it receives
//! gets a record, `<session>.<branch>.holder`, naming this host, that identity
//! and the grant itself — its fence and its owner — published whole by a
//! rename so no reader sees half of one. The kernel releases the lock when the
//! process ends, however it ends, so a later process that finds a record naming
//! exactly the active grant and can take the lock it names has proof that
//! grant's holder is gone rather than an inference from a clock. That is
//! strictly stronger than a recorded process id, which a recycled identifier
//! can make ambiguous. Records hold no descriptor, so a process holds one
//! descriptor per store directory however many branches it holds.
//!
//! A record proves only its own grant. A fence alone does not name one: a
//! grant that never committed leaves its fence unconsumed, so the next grant
//! receives the same number. The owner narrows a record to one runtime's
//! grants, but it is not unique to a process: a create's owner and fence are
//! fixed by its operation identity, and a fork's target takes its source's
//! owner, so a create or fork retried after a crash can be the very grant
//! that died. A claim therefore replaces or removes the record it finds, and
//! refuses the grant when it can do neither and the record names this grant
//! under another process.
//!
//! A lock belongs to the open file description rather than to the process, so
//! any descriptor that outlives the holder keeps it. Every descriptor here is
//! opened close-on-exec, so a child the holder spawns drops it at `exec` and a
//! surviving child cannot hold a dead holder's branches; that is pinned by
//! `a_holders_surviving_child_does_not_keep_its_branch`. A child that forked
//! without exec would still hold it, and this reads that as a live holder: no
//! proof, so the lease's own clock decides, which is the conservative answer
//! and the one that was correct before any of this existed. That is the whole
//! cost of being wrong here, and it is bounded by the lease.
//!
//! Nothing here touches the database. The directory lives beside the database
//! in the build's keyed state directory, and a holder-format change rides a
//! state-key bump, so builds of another holder format never share it.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::num::NonZeroU64;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use plakous_agent::internal::{BranchId, BranchLease, RunId, SessionId, SessionRef};
use rustix::fs::{Mode, OFlags};

const FORMAT: &str = "plak-holder-v3";

/// A record is three short lines; anything longer is not one.
const MAX_RECORD_BYTES: u64 = 512;

/// Attempts at this process's lock before claims give up for now.
const TAKE_ATTEMPTS: u32 = 3;

/// This process's lock in one holder directory, shared by every store of this
/// process that keeps claims there.
///
/// Dropping the last store closes the descriptor, which releases the lock.
/// That is the same thing process death does, so a closed store and a dead
/// process are one code path, and there is no way for a live holder to look
/// dead by forgetting to unlock.
#[derive(Debug)]
struct ProcessLock {
    file: File,
    /// The device and inode of the directory the lock was taken in.
    directory: (u64, u64),
}

/// A claim refused because the branch's record names this very grant under
/// another process, and this claim can neither replace nor remove it.
#[derive(Debug)]
pub(crate) struct StaleGrantRecord;

/// The lock this process holds in each holder directory, by the directory's
/// device and inode, while any store of this process uses it. A second open
/// of the same file would be a second open file description, whose lock
/// conflicts with the first, however the directory's path is spelled.
static PROCESS_LOCKS: Mutex<BTreeMap<(u64, u64), Weak<ProcessLock>>> = Mutex::new(BTreeMap::new());

/// The grant a claim was taken for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Claim {
    fence: NonZeroU64,
    owner: RunId,
}

/// Every branch this store currently claims, with the grant it claims.
#[derive(Debug)]
pub(crate) struct HolderClaims {
    root: Option<PathBuf>,
    lock: Option<Arc<ProcessLock>>,
    held: BTreeMap<(SessionId, BranchId), Claim>,
    sweep_pending: bool,
}

impl HolderClaims {
    /// Binds claims to the directory beside this database, without making it.
    ///
    /// The directory appears the first time this store actually takes a branch.
    /// Opening a database to read it — which retirement, inspection and repair
    /// all do — leaves nothing behind, because a directory that appeared beside
    /// every read would be an unexplained new member of the product state.
    pub(crate) fn beside(database: &Path) -> Self {
        Self {
            root: lock_root(database),
            lock: None,
            held: BTreeMap::new(),
            sweep_pending: false,
        }
    }

    /// Records this process as the holder of the grant `fence` to `owner`.
    ///
    /// Failure is not an error. The lease is the authority; this only decides
    /// whether a later process can prove the holder died, and a store that
    /// cannot record its claim simply leaves the old rule in place. A record
    /// left by another holder names another grant, so replacing it loses
    /// nothing.
    ///
    /// # Errors
    ///
    /// [`StaleGrantRecord`] when the branch's record names this very grant —
    /// its fence and owner — under another process, and this claim could
    /// neither replace nor remove it. A create retried under its operation
    /// identity has the same fence and owner as the attempt that died, so that
    /// record would prove this live grant's holder gone; the grant must not
    /// commit.
    pub(crate) fn claim(
        &mut self,
        session: SessionRef,
        fence: NonZeroU64,
        owner: RunId,
    ) -> Result<(), StaleGrantRecord> {
        let key = key(session);
        self.held.remove(&key);
        let Some(root) = self.root.clone() else {
            return Ok(());
        };
        let claim = Claim { fence, owner };
        if self.publish(&root, session, claim) {
            self.held.insert(key, claim);
            return Ok(());
        }
        let path = record_path(&root, session);
        let stuck = std::fs::remove_file(&path)
            .is_err_and(|error| error.kind() != std::io::ErrorKind::NotFound);
        if stuck
            && read_record(&path).is_some_and(|record| {
                record.identity != identity()
                    && record.fence == fence
                    && record.owner == owner.to_string()
            })
        {
            return Err(StaleGrantRecord);
        }
        Ok(())
    }

    /// Publishes the record under this process's current lock in the current
    /// directory, reporting whether it stands.
    ///
    /// The directory, or this process's lock file in it, can be replaced under
    /// a live process. A record is published only under a lock taken in the
    /// directory as it is now, and kept only if that lock is still the file
    /// its path names once the record is in place, so a record never names a
    /// lock nobody holds.
    fn publish(&mut self, root: &Path, session: SessionRef, claim: Claim) -> bool {
        for _ in 0..2 {
            if !self.lock.as_ref().is_some_and(|lock| lock.is_current(root)) {
                self.lock = None;
                let Some((lock, taken)) = process_lock(root) else {
                    return false;
                };
                self.lock = Some(lock);
                self.sweep_pending |= taken;
            }
            if write_record(root, session, claim).is_err() {
                return false;
            }
            if self.lock.as_ref().is_some_and(|lock| lock.is_current(root)) {
                return true;
            }
            let _ = std::fs::remove_file(record_path(root, session));
        }
        false
    }

    /// Runs the directory sweep the first claim of this process scheduled.
    ///
    /// Called once the granting transaction has ended: the sweep reads every
    /// record in the directory and has no business inside the writer's lock.
    pub(crate) fn sweep_if_pending(&mut self) {
        if std::mem::take(&mut self.sweep_pending)
            && let Some(root) = &self.root
        {
            sweep(root);
        }
    }

    /// Whether this store claims the branch right now.
    pub(crate) fn holds(&self, session: SessionRef) -> bool {
        self.held.contains_key(&key(session))
    }

    /// Drops this store's claim on the branch, removing its record.
    ///
    /// The directory would otherwise keep one record per branch this store
    /// ever held, for ever. Only a record that still names this process and
    /// the claimed grant is removed: a successor's record is its own proof.
    pub(crate) fn release(&mut self, session: SessionRef) {
        let Some(claim) = self.held.remove(&key(session)) else {
            return;
        };
        let Some(root) = &self.root else {
            return;
        };
        let path = record_path(root, session);
        if read_record(&path).is_some_and(|record| {
            record.identity == identity()
                && record.fence == claim.fence
                && record.owner == claim.owner.to_string()
        }) {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Drops the claim taken with `fence` once another grant has replaced it.
    ///
    /// A superseded holder's record names a grant the branch has moved past,
    /// so it proves nothing, and the claim must not let `revive` treat the
    /// superseded grant as still held.
    pub(crate) fn forfeit(&mut self, session: SessionRef, fence: NonZeroU64) {
        if self
            .held
            .get(&key(session))
            .is_some_and(|claim| claim.fence == fence)
        {
            self.release(session);
        }
    }

    /// Forgets every claim and removes every record of an archived or deleted
    /// session, whose branches no longer have grants for a record to prove.
    pub(crate) fn forget_session(&mut self, session: SessionId) {
        self.held.retain(|(held, _), _| *held != session);
        let Some(root) = &self.root else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        let prefix = format!("{session}.");
        for entry in entries.filter_map(Result::ok) {
            if entry.file_name().to_str().is_some_and(|name| {
                name.strip_prefix(&prefix)
                    .and_then(|rest| rest.strip_suffix(".holder"))
                    .is_some_and(|branch| !branch.is_empty() && !branch.contains('.'))
            }) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// Whether the holder of the branch's `active` grant is provably gone.
    ///
    /// True only on proof: the directory is this user's private directory,
    /// and the record names this host, a process that is not this one and
    /// exactly the active grant — its fence and its owner — and the named
    /// process's lock is free, which the kernel only allows once that
    /// process's last descriptor for it is closed. Everything else is
    /// `false`, which leaves the caller's existing time-bounded rule untouched.
    pub(crate) fn holder_is_gone(&self, active: &BranchLease) -> bool {
        let session = active.session();
        if self.holds(session) {
            return false;
        }
        let Some(root) = &self.root else {
            return false;
        };
        if private_directory(root).is_none() {
            return false;
        }
        let Some(record) = read_record(&record_path(root, session)) else {
            return false;
        };
        // A holder elsewhere, or this very process, is not proven dead by a
        // lock this host can take; a record of another grant proves nothing
        // about this one.
        if record.host != host()
            || record.identity == identity()
            || record.fence != active.fence()
            || record.owner != active.owner().to_string()
        {
            return false;
        }
        open_regular(&lock_path(root, &record.identity))
            .is_ok_and(|lock| lock_exclusive_nonblocking(&lock))
    }
}

#[derive(Debug)]
struct Record {
    host: String,
    identity: String,
    fence: NonZeroU64,
    owner: String,
}

fn key(session: SessionRef) -> (SessionId, BranchId) {
    (session.session_id(), session.branch_id())
}

fn lock_root(database: &Path) -> Option<PathBuf> {
    let name = database.file_name()?.to_str()?;
    Some(database.with_file_name(format!("{name}-holders")))
}

fn record_path(root: &Path, session: SessionRef) -> PathBuf {
    root.join(format!(
        "{}.{}.holder",
        session.session_id(),
        session.branch_id()
    ))
}

fn lock_path(root: &Path, identity: &str) -> PathBuf {
    root.join(format!("{identity}.lock"))
}

/// The record's contents: format, host and holder identity, then the grant.
fn record(claim: Claim) -> String {
    format!(
        "{FORMAT} {} {}\nfence {}\nowner {}\n",
        host(),
        identity(),
        claim.fence,
        claim.owner
    )
}

fn parse_record(recorded: &str) -> Option<Record> {
    let mut lines = recorded.lines();
    let (host, identity) = lines
        .next()?
        .strip_prefix(FORMAT)?
        .strip_prefix(' ')?
        .rsplit_once(' ')?;
    let fence = lines.next()?.strip_prefix("fence ")?.parse().ok()?;
    let owner = lines.next()?.strip_prefix("owner ")?;
    (is_identity(identity)
        && !owner.is_empty()
        && !owner.contains(char::is_whitespace)
        && lines.next().is_none())
    .then(|| Record {
        host: host.to_owned(),
        identity: identity.to_owned(),
        fence,
        owner: owner.to_owned(),
    })
}

fn read_record(path: &Path) -> Option<Record> {
    let mut recorded = String::new();
    open_regular(path)
        .ok()?
        .take(MAX_RECORD_BYTES)
        .read_to_string(&mut recorded)
        .ok()?;
    parse_record(&recorded)
}

/// Opens an existing regular file for reading without following a symbolic
/// link or waiting on a special file: a FIFO named like a lock or a record
/// must never block a claim inside the writer's transaction.
fn open_regular(path: &Path) -> std::io::Result<File> {
    let file = File::from(rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    if file.metadata()?.is_file() {
        Ok(file)
    } else {
        Err(std::io::ErrorKind::InvalidInput.into())
    }
}

/// Publishes the record whole: written beside it, then renamed over it.
fn write_record(root: &Path, session: SessionRef, claim: Claim) -> std::io::Result<()> {
    let path = record_path(root, session);
    let temporary = root.join(format!(
        "{}.{}.holder.{}.tmp",
        session.session_id(),
        session.branch_id(),
        identity()
    ));
    // The name is this process's own; anything left there is a leftover of
    // its own failed write, or something planted, and neither is followed.
    let _ = std::fs::remove_file(&temporary);
    let written = rustix::fs::open(
        &temporary,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(std::io::Error::from)
    .and_then(|file| File::from(file).write_all(record(claim).as_bytes()))
    .and_then(|()| std::fs::rename(&temporary, &path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

/// The writer named by a record's temporary file, if `name` is one.
fn temporary_writer(name: &str) -> Option<&str> {
    let (stem, writer) = name.strip_suffix(".tmp")?.rsplit_once('.')?;
    (stem.ends_with(".holder") && is_identity(writer)).then_some(writer)
}

/// The holder directory's device and inode, when it is a real directory owned
/// by this user that this user, and nobody else, can write. Another user could
/// plant records in anything else, and a record this user cannot remove could
/// outlive the grant it names, so claims there are unavailable and the clock
/// rules.
fn private_directory(root: &Path) -> Option<(u64, u64)> {
    let metadata = std::fs::symlink_metadata(root).ok()?;
    (metadata.is_dir()
        && metadata.uid() == rustix::process::geteuid().as_raw()
        && metadata.mode() & 0o222 == 0o200)
        .then(|| (metadata.dev(), metadata.ino()))
}

/// This process's lock in `root`, and whether this call took it. The caller
/// that took it sweeps the directory once its transaction ends.
fn process_lock(root: &Path) -> Option<(Arc<ProcessLock>, bool)> {
    // A store that cannot keep claims still works; it just waits out leases
    // the way it always did.
    match std::fs::DirBuilder::new().mode(0o700).create(root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return None,
    }
    let directory = private_directory(root)?;
    let mut locks = PROCESS_LOCKS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(lock) = locks.get(&directory).and_then(Weak::upgrade)
        && lock.is_current(root)
    {
        return Some((lock, false));
    }
    locks.retain(|_, lock| lock.strong_count() > 0);
    let lock = Arc::new(ProcessLock::take(root, directory)?);
    locks.insert(directory, Arc::downgrade(&lock));
    Some((lock, true))
}

impl ProcessLock {
    /// Whether this lock is the file this identity's lock path in `root`
    /// names, in the directory it was taken in.
    fn is_current(&self, root: &Path) -> bool {
        private_directory(root) == Some(self.directory)
            && match (
                self.file.metadata(),
                std::fs::symlink_metadata(lock_path(root, identity())),
            ) {
                (Ok(held), Ok(named)) => held.dev() == named.dev() && held.ino() == named.ino(),
                _ => false,
            }
    }

    fn take(root: &Path, directory: (u64, u64)) -> Option<Self> {
        let path = lock_path(root, identity());
        // Another process's sweep may unlink this file while it is free,
        // between this open and this lock, and a lock on an unlinked file
        // proves nothing to anyone; so the lock counts only once the path
        // still names the locked file. A checker probing this identity holds
        // its lock only for an instant, so a busy lock is retried as well.
        for attempt in 0..TAKE_ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(u64::from(attempt) * 2));
            }
            let file = File::from(
                rustix::fs::open(
                    &path,
                    OFlags::RDWR
                        | OFlags::CREATE
                        | OFlags::NONBLOCK
                        | OFlags::NOFOLLOW
                        | OFlags::CLOEXEC,
                    Mode::RUSR | Mode::WUSR,
                )
                .ok()?,
            );
            if !file.metadata().ok()?.is_file() {
                return None;
            }
            #[cfg(test)]
            tests::before_lock(&path);
            if !lock_exclusive_nonblocking(&file) {
                continue;
            }
            let (Ok(held), Ok(named)) = (file.metadata(), std::fs::symlink_metadata(&path)) else {
                continue;
            };
            if held.dev() == named.dev() && held.ino() == named.ino() {
                return Some(Self { file, directory });
            }
        }
        None
    }
}

/// Removes what dead processes left that can no longer prove anything: a
/// process lock that is free and that no record names, and a temporary record
/// whose writer's lock is free or gone.
///
/// A free lock that a record still names is kept, because it is exactly the
/// proof the next process to open that branch needs; it becomes unnamed, and
/// is swept, once that branch is claimed again.
fn sweep(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let names = entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .collect::<Vec<_>>();
    let referenced = names
        .iter()
        .filter(|name| name.ends_with(".holder"))
        .filter_map(|name| read_record(&root.join(name)))
        .map(|record| record.identity)
        .collect::<BTreeSet<_>>();
    for name in &names {
        let path = root.join(name);
        if let Some(holder) = name.strip_suffix(".lock").filter(|id| is_identity(id)) {
            if holder != identity()
                && !referenced.contains(holder)
                && open_regular(&path).is_ok_and(|lock| lock_exclusive_nonblocking(&lock))
            {
                // Removed while this sweep holds the lock, so its owner, were
                // it alive and opening it right now, sees the path change.
                let _ = std::fs::remove_file(&path);
            }
        } else if let Some(writer) = temporary_writer(name)
            && writer != identity()
            && !lock_is_held(root, writer)
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Whether `identity`'s lock is held; a lock this cannot probe counts as held.
fn lock_is_held(root: &Path, identity: &str) -> bool {
    match open_regular(&lock_path(root, identity)) {
        Ok(lock) => !lock_exclusive_nonblocking(&lock),
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}

/// Takes an exclusive lock without waiting, reporting whether it was free.
fn lock_exclusive_nonblocking(file: &File) -> bool {
    rustix::fs::flock(file, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok()
}

/// Whether `value` is shaped like a process identity: a decimal process id, a
/// hyphen and 16 lowercase hex digits. Nothing else may name a lock file, so a
/// record can never point a probe outside the directory.
fn is_identity(value: &str) -> bool {
    value.split_once('-').is_some_and(|(pid, nonce)| {
        (1..=10).contains(&pid.len())
            && pid.bytes().all(|byte| byte.is_ascii_digit())
            && nonce.len() == 16
            && nonce
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// This exact process image.
///
/// The process id is for whoever reads the directory; the random part tells
/// this process image apart from one `/reload` exec'd under the same process
/// id, and from any earlier process that had it.
fn identity() -> &'static str {
    static IDENTITY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    IDENTITY.get_or_init(|| {
        format!(
            "{}-{:016x}",
            std::process::id(),
            std::hash::BuildHasher::hash_one(&std::hash::RandomState::new(), std::process::id())
        )
    })
}

/// This host, compared so a holder on another machine is never read as dead,
/// which matters when a database is shared.
fn host() -> String {
    rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test fixtures fail at exact lines")]
mod tests;
