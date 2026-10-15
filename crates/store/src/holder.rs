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
use std::num::NonZeroU64;
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
    fence: NonZeroU64,
    _file: File,
}

/// Most claims one store holds at once. Each held claim keeps a descriptor
/// open, so without a bound enough concurrent leases exhaust the process's
/// descriptor limit and the database's own temporary files fail to open.
/// Claims past the bound are skipped like any other failed claim.
const MAX_HELD_CLAIMS: usize = 128;

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

    /// Takes the branch's lock, recording who holds it and under which fence.
    ///
    /// Failure is not an error. The lease is the authority; this only decides
    /// whether a later process can prove the holder died, and a store that
    /// cannot take the lock simply leaves the old rule in place. The file is
    /// rewritten only once the lock is held: a claim that loses to a live
    /// holder leaves that holder's record exactly as it was.
    pub(crate) fn claim(&mut self, session: SessionRef, fence: NonZeroU64) {
        let key = (session.session_id(), session.branch_id());
        self.held.remove(&key);
        if self.held.len() >= MAX_HELD_CLAIMS {
            return;
        }
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
            .truncate(false)
            .mode(0o600)
            .open(&path)
        else {
            return;
        };
        if !lock_exclusive_nonblocking(&file) {
            return;
        }
        if file.set_len(0).is_err() || file.write_all(record(fence).as_bytes()).is_err() {
            return;
        }
        let _ = file.flush();
        self.held.insert(key, HolderClaim { fence, _file: file });
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

    /// Drops the claim taken with `fence` once another grant has replaced it.
    ///
    /// A superseded holder must not keep pinning a lock it no longer owns: its
    /// record names a fence the branch has moved past, so it proves nothing,
    /// and holding it only stops the new holder from ever taking the lock.
    pub(crate) fn forfeit(&mut self, session: SessionRef, fence: NonZeroU64) {
        let key = (session.session_id(), session.branch_id());
        if self
            .held
            .get(&key)
            .is_some_and(|claim| claim.fence == fence)
        {
            self.release(session);
        }
    }

    /// Whether the holder of the branch's `active` grant is provably gone.
    ///
    /// True only on proof: the lock file names this host, a process that is not
    /// this one and exactly the active grant's fence, and its lock is free,
    /// which the kernel only allows once the holder's last descriptor is
    /// closed. A record naming an earlier grant says nothing about the current
    /// holder, which may never have taken the lock at all. Everything else is
    /// `false`, which leaves the caller's existing time-bounded rule untouched.
    pub(crate) fn holder_is_gone(&self, session: SessionRef, active: NonZeroU64) -> bool {
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
        let Some((holder, fence)) = parse_record(&recorded) else {
            return false;
        };
        // A holder elsewhere, or this very process, is not proven dead by a
        // lock this host can take.
        if holder == identity() || !holder.starts_with(&host_prefix()) || fence != active {
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

/// The lock file's contents: the holder's identity, then the fence it holds.
fn record(fence: NonZeroU64) -> String {
    format!("{}\nfence {fence}\n", identity())
}

fn parse_record(recorded: &str) -> Option<(&str, NonZeroU64)> {
    let mut lines = recorded.lines();
    let holder = lines.next()?;
    let fence = lines.next()?.strip_prefix("fence ")?.parse().ok()?;
    lines.next().is_none().then_some((holder, fence))
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
    format!("plak-holder-v2 {host} ")
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test fixtures fail at exact lines")]
mod tests {
    use std::num::NonZeroU64;
    use std::path::{Path, PathBuf};

    use super::{HolderClaims, host_prefix};
    use plakous_agent::internal::SessionRef;
    use plakous_model::{BranchId, SessionId};

    fn session() -> SessionRef {
        SessionRef::new(
            SessionId::try_from(uuid::Uuid::from_u128(0x5e55_1041)).expect("session"),
            BranchId::try_from(uuid::Uuid::from_u128(0xb4a4_c401)).expect("branch"),
        )
    }

    fn fence(value: u64) -> NonZeroU64 {
        NonZeroU64::new(value).expect("fence")
    }

    fn lock_path(directory: &Path) -> PathBuf {
        directory.join("runtime.sqlite3-holders").join(format!(
            "{}.{}.lock",
            session().session_id(),
            session().branch_id()
        ))
    }

    /// Only proof counts. A store written before locks existed has no file, a
    /// holder on another machine has one this host cannot speak for, a branch
    /// this very store holds is obviously not free, and a record naming an
    /// earlier grant says nothing about the current one. None of those is a
    /// dead holder, and each keeps the lease's own clock.
    #[test]
    fn nothing_short_of_a_free_lock_naming_the_active_grant_proves_a_holder_died() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = directory.path().join("runtime.sqlite3");

        // A store from before locks existed: no file at all.
        let claims = HolderClaims::beside(&database);
        assert!(
            !claims.holder_is_gone(session(), fence(1)),
            "an absent lock is an unknown holder, not a dead one"
        );

        // A holder this store is itself holding.
        let mut holding = HolderClaims::beside(&database);
        holding.claim(session(), fence(1));
        assert!(
            !holding.holder_is_gone(session(), fence(1)),
            "a branch this store holds is not free"
        );

        // A holder on another machine, whose lock this host can take and must
        // not read anything into.
        holding.release(session());
        let path = lock_path(directory.path());
        std::fs::write(&path, "plak-holder-v2 some-other-host 4321 0\nfence 1\n")
            .expect("foreign lock");
        assert!(
            !claims.holder_is_gone(session(), fence(1)),
            "a holder on another host is not proven dead by a lock this host can take"
        );

        // The same file, now naming this host and a process that is not ours.
        std::fs::write(&path, format!("{}999999 0\nfence 1\n", host_prefix())).expect("local lock");
        assert!(
            claims.holder_is_gone(session(), fence(1)),
            "a free lock naming this host, another process and the active grant is proof"
        );
        assert!(
            !claims.holder_is_gone(session(), fence(2)),
            "a free lock naming an earlier grant says nothing about the active one"
        );

        // A holder that died between taking its lock and naming itself, or a
        // record this build cannot read, names no grant at all.
        for unreadable in ["", "plak-holder-v1 host 1 0", "garbage\nfence x\n"] {
            std::fs::write(&path, unreadable).expect("unreadable lock");
            assert!(
                !claims.holder_is_gone(session(), fence(1)),
                "an unreadable record {unreadable:?} is not proof"
            );
        }
    }

    /// A claim that loses to a live holder leaves that holder's record alone.
    ///
    /// Otherwise a process that acquired a lease whose suspended holder still
    /// had the lock would wipe the holder's name, and once that holder exited a
    /// third process would read the empty, unlocked file as proof and take the
    /// second process's live lease.
    #[test]
    fn a_claim_that_loses_the_lock_leaves_the_holders_record_untouched() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = directory.path().join("runtime.sqlite3");
        let path = lock_path(directory.path());

        let mut first = HolderClaims::beside(&database);
        first.claim(session(), fence(1));
        let recorded = std::fs::read_to_string(&path).expect("first record");
        assert!(
            recorded.ends_with("\nfence 1\n"),
            "the record names its grant"
        );

        let mut second = HolderClaims::beside(&database);
        second.claim(session(), fence(2));
        assert!(!second.holds(session()), "a held lock is not taken twice");
        assert_eq!(
            std::fs::read_to_string(&path).expect("record after a lost claim"),
            recorded,
            "a lost claim rewrites nothing"
        );

        // Only the superseded grant's claim is forfeited, and then the lock is
        // free for the current holder.
        first.forfeit(session(), fence(2));
        assert!(
            first.holds(session()),
            "a different grant's forfeit is ignored"
        );
        first.forfeit(session(), fence(1));
        assert!(!first.holds(session()), "the superseded claim is dropped");
        second.claim(session(), fence(2));
        assert!(
            second.holds(session()),
            "the current holder can now take the lock"
        );
        assert!(
            std::fs::read_to_string(&path)
                .expect("second record")
                .ends_with("\nfence 2\n")
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
        let path = lock_path(directory.path());

        let mut claims = HolderClaims::beside(&database);
        claims.claim(session(), fence(1));
        assert!(path.exists(), "a held branch has a lock file");
        claims.release(session());
        assert!(!path.exists(), "a released branch leaves nothing behind");
    }

    /// Held claims are bounded, so leases can never take the descriptors the
    /// database itself needs. A claim past the bound is skipped like any other
    /// claim that fails, and a released claim frees its slot.
    #[test]
    fn held_claims_stop_at_their_descriptor_budget() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = directory.path().join("runtime.sqlite3");
        let branch = |n: u128| {
            SessionRef::new(
                session().session_id(),
                BranchId::try_from(uuid::Uuid::from_u128(0xb4a4_0000 + n)).expect("branch"),
            )
        };
        let mut claims = HolderClaims::beside(&database);
        for n in 0..=128 {
            claims.claim(branch(n), fence(1));
        }
        let held = (0..=128).filter(|&n| claims.holds(branch(n))).count();
        assert_eq!(held, 128, "claims past the budget are skipped");
        assert!(!claims.holds(branch(128)));
        claims.release(branch(0));
        claims.claim(branch(128), fence(1));
        assert!(claims.holds(branch(128)), "a released claim frees its slot");
        claims.claim(branch(127), fence(2));
        assert!(
            claims.holds(branch(127)),
            "re-claiming a held branch keeps its slot"
        );
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
