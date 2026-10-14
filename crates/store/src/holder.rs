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
//! So the holder also takes an exclusive advisory lock on a file beside the
//! database, and holds it for the life of the lease. The kernel releases that
//! lock when the process ends, however it ends, so a later process that can
//! take the lock has proof the holder is gone rather than an inference from a
//! clock. That is strictly stronger than a recorded process identity, which a
//! recycled identifier can make ambiguous.
//!
//! A lock belongs to the open file description rather than to the process, so
//! any descriptor that outlives the holder keeps it. Rust opens files
//! close-on-exec, so a child the holder spawns drops it at `exec` and a
//! surviving child cannot hold a dead holder's branch; that is pinned by
//! `a_holders_surviving_child_does_not_keep_its_branch`. A child that forked
//! without exec would still hold it, and this reads that as a live holder: no
//! proof, so the lease's own clock decides, which is the conservative answer
//! and the one that was correct before any of this existed. That is the whole
//! cost of being wrong here, and it is bounded by the lease.
//!
//! Nothing here touches the database. A store written by an earlier build has
//! no lock files, which reads as "unknown holder" and keeps the time-bounded
//! rule, so the persisted bytes and the schema are exactly what they were.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use plakous_agent::internal::{BranchId, SessionId, SessionRef};

/// One held lock, kept alive for exactly as long as its lease.
///
/// Dropping the file closes the descriptor, which releases the lock. That is
/// the same thing process death does, so release and death are one code path
/// and there is no way for a live holder to look dead by forgetting to unlock.
#[derive(Debug)]
pub(crate) struct HolderClaim {
    _file: File,
}

/// Every claim this store currently holds, by branch.
#[derive(Debug, Default)]
pub(crate) struct HolderClaims {
    root: Option<PathBuf>,
    held: BTreeMap<(SessionId, BranchId), HolderClaim>,
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
            held: BTreeMap::new(),
        }
    }

    /// Takes the branch's lock, recording who holds it.
    ///
    /// Failure is not an error. The lease is the authority; this only decides
    /// whether a later process can prove the holder died, and a store that
    /// cannot take the lock simply leaves the old rule in place.
    pub(crate) fn claim(&mut self, session: SessionRef) {
        let key = (session.session_id(), session.branch_id());
        self.held.remove(&key);
        let Some(path) = self.path(session) else {
            return;
        };
        if let Some(parent) = path.parent()
            && std::fs::create_dir_all(parent).is_err()
        {
            // A store that cannot keep locks still works; it just waits out
            // leases the way it always did.
            return;
        }
        let Ok(mut file) = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
        else {
            return;
        };
        if !lock_exclusive_nonblocking(&file) {
            return;
        }
        if file.write_all(identity().as_bytes()).is_err() {
            return;
        }
        let _ = file.flush();
        self.held.insert(key, HolderClaim { _file: file });
    }

    /// Whether this store holds the branch's lock right now.
    pub(crate) fn holds(&self, session: SessionRef) -> bool {
        self.held
            .contains_key(&(session.session_id(), session.branch_id()))
    }

    /// Releases the branch's lock, if this store holds it, and removes its file.
    ///
    /// The directory would otherwise keep one file per branch this store ever
    /// held, for ever. Removing it on release is safe because the lock, not the
    /// file, is what decides: a file that reappears unlocked reads as a holder
    /// that is gone, which is exactly what a released branch is.
    pub(crate) fn release(&mut self, session: SessionRef) {
        let held = self
            .held
            .remove(&(session.session_id(), session.branch_id()))
            .is_some();
        if held && let Some(path) = self.path(session) {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Whether the recorded holder of this branch is provably gone.
    ///
    /// True only on proof: the lock exists, names this host, names a process
    /// that is not this one, and is free, which the kernel only allows once the
    /// holder's last descriptor is closed. Everything else is `false`, which
    /// leaves the caller's existing time-bounded rule untouched.
    pub(crate) fn holder_is_gone(&self, session: SessionRef) -> bool {
        if self
            .held
            .contains_key(&(session.session_id(), session.branch_id()))
        {
            return false;
        }
        let Some(path) = self.path(session) else {
            return false;
        };
        let Ok(mut file) = OpenOptions::new().read(true).write(true).open(&path) else {
            return false;
        };
        let mut recorded = String::new();
        if file.read_to_string(&mut recorded).is_err() {
            return false;
        }
        let recorded = recorded.trim();
        // A holder elsewhere, or one this build cannot read, is not proven
        // dead by a lock this host can take. An empty file is: a holder that
        // died between creating its file and writing its name is still a holder
        // that died, and the lock it no longer holds says so.
        if recorded == identity() || (!recorded.is_empty() && !recorded.starts_with(&host_prefix()))
        {
            return false;
        }
        lock_exclusive_nonblocking(&file)
    }

    fn path(&self, session: SessionRef) -> Option<PathBuf> {
        self.root.as_ref().map(|root| {
            root.join(format!(
                "{}.{}.lock",
                session.session_id(),
                session.branch_id()
            ))
        })
    }
}

fn lock_root(database: &Path) -> Option<PathBuf> {
    let name = database.file_name()?.to_str()?;
    Some(database.with_file_name(format!("{name}-holders")))
}

/// Takes an exclusive lock without waiting, reporting whether it was free.
fn lock_exclusive_nonblocking(file: &File) -> bool {
    rustix::fs::flock(file, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok()
}

/// This exact process image, on this exact host.
///
/// The host is compared so a holder on another machine is never read as dead,
/// which matters when a database is shared. The process is compared so a second
/// session inside this same process is not either: this process can always take
/// a lock it already holds. The random suffix tells this process image apart
/// from one `/reload` exec'd under the same process id.
fn identity() -> String {
    static IDENTITY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    IDENTITY
        .get_or_init(|| {
            format!(
                "{}{} {:016x}",
                host_prefix(),
                std::process::id(),
                std::hash::BuildHasher::hash_one(
                    &std::hash::RandomState::new(),
                    std::process::id()
                )
            )
        })
        .clone()
}

fn host_prefix() -> String {
    let host = rustix::system::uname()
        .nodename()
        .to_str()
        .unwrap_or("unknown-host")
        .to_owned();
    format!("plak-holder-v1 {host} ")
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test fixtures fail at exact lines")]
mod tests {
    use super::{HolderClaims, host_prefix};
    use plakous_agent::internal::SessionRef;
    use plakous_model::{BranchId, SessionId};

    fn session() -> SessionRef {
        SessionRef::new(
            SessionId::try_from(uuid::Uuid::from_u128(0x5e55_1041)).expect("session"),
            BranchId::try_from(uuid::Uuid::from_u128(0xb4a4_c401)).expect("branch"),
        )
    }

    /// Only proof counts. A store written before locks existed has no file, a
    /// holder on another machine has one this host cannot speak for, and a
    /// branch this very store holds is obviously not free. None of those is a
    /// dead holder, and each keeps the lease's own clock.
    #[test]
    fn nothing_short_of_a_free_lock_on_this_host_proves_a_holder_died() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = directory.path().join("runtime.sqlite3");

        // A store from before locks existed: no file at all.
        let claims = HolderClaims::beside(&database);
        assert!(
            !claims.holder_is_gone(session()),
            "an absent lock is an unknown holder, not a dead one"
        );

        // A holder this store is itself holding.
        let mut holding = HolderClaims::beside(&database);
        holding.claim(session());
        assert!(
            !holding.holder_is_gone(session()),
            "a branch this store holds is not free"
        );

        // A holder on another machine, whose lock this host can take and must
        // not read anything into.
        holding.release(session());
        let path = directory
            .path()
            .join("runtime.sqlite3-holders")
            .join(format!(
                "{}.{}.lock",
                session().session_id(),
                session().branch_id()
            ));
        std::fs::write(&path, "plak-holder-v1 some-other-host 4321").expect("foreign lock");
        assert!(
            !claims.holder_is_gone(session()),
            "a holder on another host is not proven dead by a lock this host can take"
        );

        // The same file, now naming this host and a process that is not ours.
        std::fs::write(&path, format!("{}999999", host_prefix())).expect("local lock");
        assert!(
            claims.holder_is_gone(session()),
            "a free lock naming this host and another process is proof"
        );

        // A holder that died between creating its file and naming itself. The
        // lock decides, not the file, so an unlocked leftover is still a holder
        // that is gone.
        std::fs::write(&path, "").expect("empty lock");
        assert!(
            claims.holder_is_gone(session()),
            "a stale file with no holder is not a reason to wait"
        );
    }

    /// Releasing a branch takes its file with it.
    ///
    /// Otherwise the directory keeps one file per branch this store ever held,
    /// for ever. Removing it is safe because the lock is what decides: a file
    /// that came back unlocked would read as a holder that is gone, which is
    /// what a released branch is.
    #[test]
    fn releasing_a_branch_removes_its_lock_file() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = directory.path().join("runtime.sqlite3");
        let path = directory
            .path()
            .join("runtime.sqlite3-holders")
            .join(format!(
                "{}.{}.lock",
                session().session_id(),
                session().branch_id()
            ));

        let mut claims = HolderClaims::beside(&database);
        claims.claim(session());
        assert!(path.exists(), "a held branch has a lock file");
        claims.release(session());
        assert!(!path.exists(), "a released branch leaves nothing behind");
    }

    /// Opening a database leaves nothing beside it.
    ///
    /// Retirement, inspection and repair all open a store only to read it, and
    /// a directory appearing beside every read would be an unexplained new
    /// member of the product state that the retirement staging check refuses.
    #[test]
    fn binding_claims_creates_nothing_until_a_branch_is_taken() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = directory.path().join("runtime.sqlite3");
        let _claims = HolderClaims::beside(&database);
        assert!(
            !directory.path().join("runtime.sqlite3-holders").exists(),
            "reading a store leaves nothing beside it"
        );
    }
}
