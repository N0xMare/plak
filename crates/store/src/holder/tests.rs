use std::cell::Cell;
use std::fs::File;
use std::num::NonZeroU64;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{FORMAT, HolderClaims, host, identity};
use plakous_agent::internal::{BranchLease, LeaseDomain, LeaseInstant, RunId, SessionRef};
use plakous_model::{BranchId, SessionId};

thread_local! {
    /// Runs once, between this process opening its lock file and locking it.
    static BEFORE_LOCK: Cell<Option<fn(&Path)>> = const { Cell::new(None) };
}

pub(super) fn before_lock(path: &Path) {
    if let Some(hook) = BEFORE_LOCK.with(Cell::take) {
        hook(path);
    }
}

fn session() -> SessionRef {
    branch(0)
}

fn branch(n: u128) -> SessionRef {
    SessionRef::new(
        SessionId::try_from(uuid::Uuid::from_u128(0x5e55_1041)).expect("session"),
        BranchId::try_from(uuid::Uuid::from_u128(0xb4a4_c401 + n)).expect("branch"),
    )
}

fn other_session() -> SessionRef {
    SessionRef::new(
        SessionId::try_from(uuid::Uuid::from_u128(0x5e55_2041)).expect("session"),
        BranchId::try_from(uuid::Uuid::from_u128(0xb4a4_c401)).expect("branch"),
    )
}

fn fence(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).expect("fence")
}

fn owner(n: u128) -> RunId {
    RunId::try_from(uuid::Uuid::from_u128(0x0e4e_0000 + n)).expect("owner")
}

/// The active grant of `session` under `fence` to `owner`.
fn lease(session: SessionRef, fence: u64, owner: RunId) -> BranchLease {
    BranchLease::from_store(
        session,
        owner,
        self::fence(fence),
        LeaseInstant::new(LeaseDomain::new([1; 16], [2; 32]), 100, 0).expect("expiry"),
        self::fence(30),
    )
}

fn holders(directory: &Path) -> PathBuf {
    directory.join("runtime.sqlite3-holders")
}

fn stem(session: SessionRef) -> String {
    format!("{}.{}", session.session_id(), session.branch_id())
}

fn record_path(directory: &Path, session: SessionRef) -> PathBuf {
    holders(directory).join(format!("{}.holder", stem(session)))
}

fn lock_path(directory: &Path, identity: &str) -> PathBuf {
    holders(directory).join(format!("{identity}.lock"))
}

/// The record a process on this host writes.
fn local_record(identity: &str, fence: u64, owner: RunId) -> String {
    format!(
        "{FORMAT} {} {identity}\nfence {fence}\nowner {owner}\n",
        host()
    )
}

fn private_holders(directory: &Path) {
    std::fs::create_dir_all(holders(directory)).expect("holder directory");
    std::fs::set_permissions(holders(directory), std::fs::Permissions::from_mode(0o700))
        .expect("private holder directory");
}

/// Plants another process's claim: its lock file, and its record for
/// `session` under `fence` to `owner`. The lock is free unless the caller
/// locks it.
fn plant(directory: &Path, identity: &str, session: SessionRef, fence: u64, owner: RunId) -> File {
    private_holders(directory);
    std::fs::write(
        record_path(directory, session),
        local_record(identity, fence, owner),
    )
    .expect("record");
    File::create(lock_path(directory, identity)).expect("lock file")
}

fn lock(file: &File) {
    rustix::fs::flock(file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .expect("test lock");
}

fn is_locked(path: &Path) -> bool {
    let file = File::open(path).expect("lock file");
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err()
}

fn mkfifo(path: &Path) {
    let status = std::process::Command::new("mkfifo")
        .arg(path)
        .status()
        .expect("mkfifo");
    assert!(status.success(), "mkfifo {}", path.display());
}

fn names(directory: &Path) -> Vec<String> {
    let mut names = std::fs::read_dir(holders(directory))
        .expect("holder directory")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .into_string()
                .expect("name")
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn open_descriptors() -> usize {
    std::fs::read_dir("/dev/fd")
        .expect("descriptor table")
        .count()
}

/// Only proof counts. A store written before records existed has none, a
/// holder on another machine has one this host cannot speak for, a branch
/// this very process holds is obviously not free, a record of another grant
/// says nothing about the current one, and a live holder keeps its lock.
/// None of those is a dead holder, and each keeps the lease's clock.
#[test]
fn nothing_short_of_a_free_lock_named_by_the_active_grant_proves_a_holder_died() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("runtime.sqlite3");
    let claims = HolderClaims::beside(&database);
    let active = lease(session(), 2, owner(1));

    // A store from before records existed: no file at all.
    assert!(
        !claims.holder_is_gone(&lease(session(), 1, owner(1))),
        "an absent record is an unknown holder, not a dead one"
    );

    // A holder this store is itself holding, and another store of this same
    // process reading the record that names this process.
    let mut holding = HolderClaims::beside(&database);
    holding.claim(session(), fence(1), owner(1)).expect("claim");
    assert!(holding.holds(session()));
    assert!(
        !holding.holder_is_gone(&lease(session(), 1, owner(1))),
        "a branch this store holds is not free"
    );
    assert!(
        !claims.holder_is_gone(&lease(session(), 1, owner(1))),
        "this process is not proven dead by a lock it holds itself"
    );
    holding.release(session());

    // A dead process on this host whose record names the active grant.
    let dead = "999999-00000000000000aa";
    let lock_file = plant(directory.path(), dead, session(), 2, owner(1));
    assert!(
        claims.holder_is_gone(&active),
        "a free lock named by this host, another process and the active grant is proof"
    );

    // A stale record: the holder it names held an earlier grant.
    assert!(
        !claims.holder_is_gone(&lease(session(), 3, owner(1))),
        "a record naming an earlier fence says nothing about the active grant"
    );

    // The same fence granted to another owner: the record's grant never
    // committed, and the live grant reused its fence.
    assert!(
        !claims.holder_is_gone(&lease(session(), 2, owner(2))),
        "a record naming another owner under the active fence is not proof"
    );

    // The same record while its process still holds its lock.
    lock(&lock_file);
    assert!(
        !claims.holder_is_gone(&active),
        "a held lock is a live holder"
    );
    drop(lock_file);
    assert!(claims.holder_is_gone(&active));

    // A record whose lock file is gone proves nothing.
    std::fs::remove_file(lock_path(directory.path(), dead)).expect("remove lock");
    assert!(
        !claims.holder_is_gone(&active),
        "a record without its lock file is not proof"
    );
    drop(plant(directory.path(), dead, session(), 2, owner(1)));

    // A holder on another machine, whose lock this host can take and must
    // not read anything into.
    std::fs::write(
        record_path(directory.path(), session()),
        format!(
            "{FORMAT} some-other-host {dead}\nfence 2\nowner {}\n",
            owner(1)
        ),
    )
    .expect("foreign record");
    assert!(
        !claims.holder_is_gone(&active),
        "a holder on another host is not proven dead by a lock this host can take"
    );

    // A record this build cannot read names no grant at all.
    let host = host();
    let owner = owner(1);
    for unreadable in [
        String::new(),
        format!("plak-holder-v2 {host} {dead}\nfence 2\n"),
        format!("{FORMAT} {host} {dead}\nfence 2\n"),
        format!("{FORMAT} {host} {dead}\nfence x\nowner {owner}\n"),
        format!("{FORMAT} {host} {dead}\nfence 2\nowner \n"),
        format!("{FORMAT} {host} {dead}\nfence 2\nowner {owner}\nmore\n"),
        format!("{FORMAT} {host} 999999-00000000000000AA\nfence 2\nowner {owner}\n"),
        format!("{FORMAT} {host} {dead}"),
        format!("{}\n", "x".repeat(4096)),
    ] {
        std::fs::write(record_path(directory.path(), session()), &unreadable)
            .expect("unreadable record");
        assert!(
            !claims.holder_is_gone(&active),
            "an unreadable record {unreadable:?} is not proof"
        );
    }
}

/// A record cannot point the probe outside the directory: a free lock at
/// the traversal target of `../<identity>` is not proof.
#[test]
fn a_record_cannot_point_its_probe_outside_the_directory() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("runtime.sqlite3");
    let dead = "999999-00000000000000ab";
    private_holders(directory.path());
    drop(File::create(directory.path().join(format!("{dead}.lock"))).expect("outside lock"));
    std::fs::write(
        record_path(directory.path(), session()),
        local_record(&format!("../{dead}"), 2, owner(1)),
    )
    .expect("traversing record");
    assert!(
        !HolderClaims::beside(&database).holder_is_gone(&lease(session(), 2, owner(1))),
        "a record naming a path outside the directory is not proof"
    );
}

/// A store this process dropped left its record and freed its lock; another
/// store of the same process still must not read that as a dead holder.
#[test]
fn a_dropped_stores_record_is_not_proof_inside_its_own_process() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("runtime.sqlite3");
    let mut dropped = HolderClaims::beside(&database);
    dropped.claim(session(), fence(1), owner(1)).expect("claim");
    drop(dropped);
    assert!(
        !is_locked(&lock_path(directory.path(), identity())),
        "the last store's drop freed the lock"
    );
    assert!(
        !HolderClaims::beside(&database).holder_is_gone(&lease(session(), 1, owner(1))),
        "this process is alive"
    );
}

/// A claim always replaces the branch's record with one naming its own grant,
/// and only the record's own holder removes it.
///
/// The holder of an earlier grant, still alive and later finding itself
/// superseded, must not take its successor's record with it, or the
/// successor's death would leave nothing to prove it.
#[test]
fn a_claim_replaces_the_record_and_a_superseded_holder_leaves_its_successors() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("runtime.sqlite3");
    let path = record_path(directory.path(), session());

    let mut first = HolderClaims::beside(&database);
    first.claim(session(), fence(1), owner(1)).expect("claim");
    assert_eq!(
        std::fs::read_to_string(&path).expect("first record"),
        local_record(identity(), 1, owner(1)),
        "the record names this host, this process and its grant"
    );

    let mut second = HolderClaims::beside(&database);
    second.claim(session(), fence(2), owner(2)).expect("claim");
    assert!(second.holds(session()));
    assert_eq!(
        std::fs::read_to_string(&path).expect("second record"),
        local_record(identity(), 2, owner(2)),
        "a claim replaces an earlier grant's record"
    );

    first.forfeit(session(), fence(2));
    assert!(
        first.holds(session()),
        "a different grant's forfeit is ignored"
    );
    first.forfeit(session(), fence(1));
    assert!(!first.holds(session()), "the superseded claim is dropped");
    assert_eq!(
        std::fs::read_to_string(&path).expect("record after forfeit"),
        local_record(identity(), 2, owner(2)),
        "the superseded holder leaves its successor's record"
    );
    first.release(session());
    assert!(path.exists(), "releasing an unheld branch removes nothing");

    second.release(session());
    assert!(!path.exists(), "a released branch leaves no record behind");
    assert!(!second.holds(session()));
}

/// A dead claimant's record can never prove a later grant's holder gone.
///
/// A grant that never committed leaves its fence unconsumed, so a holder that
/// died inside one leaves a record naming the fence the next grant receives.
/// When the next claimant cannot replace or remove it — here its directory
/// is not writable — the record still names the dead claimant's own owner,
/// not the live grant's.
#[test]
fn a_dead_claimants_record_never_matches_a_later_grant_of_its_fence() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("runtime.sqlite3");
    let dead = "999999-00000000000000bb";
    drop(plant(directory.path(), dead, session(), 4, owner(1)));
    std::fs::set_permissions(
        holders(directory.path()),
        std::fs::Permissions::from_mode(0o500),
    )
    .expect("read-only holder directory");

    let mut live = HolderClaims::beside(&database);
    live.claim(session(), fence(4), owner(2)).expect("claim");
    let checker = HolderClaims::beside(&database);
    let refused = !checker.holder_is_gone(&lease(session(), 4, owner(2)));
    std::fs::set_permissions(
        holders(directory.path()),
        std::fs::Permissions::from_mode(0o700),
    )
    .expect("writable holder directory");
    assert!(
        refused,
        "the dead claimant's record does not name the live grant's owner"
    );
}

/// A claim that cannot publish its record leaves no record behind.
#[test]
fn a_failed_publish_removes_the_old_record() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("runtime.sqlite3");
    let dead = "999999-00000000000000bc";
    drop(plant(directory.path(), dead, session(), 4, owner(1)));

    // The claimant's temporary record cannot be created.
    std::fs::create_dir(holders(directory.path()).join(format!(
        "{}.holder.{}.tmp",
        stem(session()),
        identity()
    )))
    .expect("obstruction");
    let mut claims = HolderClaims::beside(&database);
    claims.claim(session(), fence(4), owner(2)).expect("claim");
    assert!(!claims.holds(session()), "the claim failed");
    assert!(!record_path(directory.path(), session()).exists());
}

/// Without this process's lock, a claim publishes nothing: a record naming a
/// lock nobody holds would read as a dead holder.
#[test]
fn a_claim_without_the_process_lock_publishes_no_record() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("runtime.sqlite3");
    private_holders(directory.path());
    std::fs::create_dir(lock_path(directory.path(), identity())).expect("obstruction");
    let mut claims = HolderClaims::beside(&database);
    claims.claim(session(), fence(1), owner(1)).expect("claim");
    assert!(!claims.holds(session()));
    assert!(!record_path(directory.path(), session()).exists());
}

/// A sweep that unlinks this process's lock file between its open and its
/// lock leaves a lock on a file nobody can find; the claim notices and takes
/// the lock on the file the path names.
#[test]
fn a_lock_file_unlinked_before_it_is_locked_is_taken_again() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("runtime.sqlite3");
    BEFORE_LOCK.with(|hook| {
        hook.set(Some(|path| {
            std::fs::remove_file(path).expect("unlink lock");
        }));
    });
    let mut claims = HolderClaims::beside(&database);
    claims.claim(session(), fence(1), owner(1)).expect("claim");
    assert!(claims.holds(session()));
    assert!(
        is_locked(&lock_path(directory.path(), identity())),
        "the lock the record names is held"
    );
}

/// A directory another user could write is no place for proof: claims there
/// are unavailable, and records there are not read.
#[test]
fn a_holder_directory_others_can_write_holds_no_proof() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("runtime.sqlite3");
    let dead = "999999-00000000000000bd";
    drop(plant(directory.path(), dead, session(), 1, owner(1)));
    std::fs::set_permissions(
        holders(directory.path()),
        std::fs::Permissions::from_mode(0o770),
    )
    .expect("group-writable holder directory");
    let mut claims = HolderClaims::beside(&database);
    assert!(!claims.holder_is_gone(&lease(session(), 1, owner(1))));
    claims.claim(branch(1), fence(1), owner(1)).expect("claim");
    assert!(!claims.holds(branch(1)));
    assert!(!record_path(directory.path(), branch(1)).exists());
}

/// Special files named like locks or records are skipped, never waited on.
#[test]
fn special_files_named_like_holder_files_never_block() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("runtime.sqlite3");
    private_holders(directory.path());
    mkfifo(&record_path(directory.path(), branch(1)));
    mkfifo(&lock_path(directory.path(), "999999-00000000000000be"));
    let mut claims = HolderClaims::beside(&database);
    assert!(!claims.holder_is_gone(&lease(branch(1), 1, owner(1))));
    claims.claim(session(), fence(1), owner(1)).expect("claim");
    claims.sweep_if_pending();
    assert!(claims.holds(session()));

    let other = tempfile::tempdir().expect("temporary directory");
    private_holders(other.path());
    mkfifo(&lock_path(other.path(), identity()));
    let mut blocked = HolderClaims::beside(&other.path().join("runtime.sqlite3"));
    blocked.claim(session(), fence(1), owner(1)).expect("claim");
    assert!(
        !blocked.holds(session()),
        "a FIFO at this process's lock path is not a lock"
    );
}

/// One process holds one descriptor however many branches it claims.
///
/// Records hold no descriptor, so leases never take the descriptors the
/// database itself needs, and there is no bound past which claims stop.
#[test]
fn many_branches_in_one_process_hold_one_descriptor() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("runtime.sqlite3");
    let before = open_descriptors();
    let mut claims = HolderClaims::beside(&database);
    // The same directory spelled another way is the same lock.
    let mut other = HolderClaims::beside(&directory.path().join(".").join("runtime.sqlite3"));
    for n in 0..1_000 {
        claims.claim(branch(n), fence(1), owner(1)).expect("claim");
        other
            .claim(branch(10_000 + n), fence(1), owner(2))
            .expect("claim");
    }
    assert!((0..1_000).all(|n| claims.holds(branch(n)) && other.holds(branch(10_000 + n))));
    assert_eq!(
        open_descriptors(),
        before + 1,
        "two stores and two thousand claims share one process lock"
    );
    let names = names(directory.path());
    assert_eq!(
        names
            .iter()
            .filter(|name| name.strip_suffix(".lock").is_some())
            .collect::<Vec<_>>(),
        [&format!("{}.lock", identity())],
        "one process, one lock file"
    );
    assert_eq!(
        names
            .iter()
            .filter(|name| name.ends_with(".holder"))
            .count(),
        2_000
    );
    drop(claims);
    drop(other);
    assert_eq!(open_descriptors(), before, "the last store closes the lock");
}

/// The first claim in a directory sweeps what dead processes left behind and
/// nothing else.
///
/// A free lock no record names, and a temporary record whose writer is gone,
/// can never prove anything again. A free lock a record still names is the
/// proof the next process to open that branch needs. A live process's files,
/// and names this format never writes, are not this sweep's to touch.
#[test]
fn the_startup_sweep_removes_only_what_dead_processes_can_no_longer_use() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("runtime.sqlite3");
    let root = holders(directory.path());
    private_holders(directory.path());
    let other = stem(branch(9));

    let unnamed_dead = "999999-00000000000000c1";
    let named_dead = "999999-00000000000000c2";
    let live = "999999-00000000000000c3";
    let gone = "999999-00000000000000c4";
    drop(File::create(lock_path(directory.path(), unnamed_dead)).expect("dead lock"));
    drop(plant(directory.path(), named_dead, branch(7), 3, owner(7)));
    let live_lock = File::create(lock_path(directory.path(), live)).expect("live lock");
    lock(&live_lock);
    let live_temporary = format!("{other}.holder.{live}.tmp");
    let foreign_name = format!("{other}.lock");
    for name in [
        live_temporary.clone(),
        format!("{other}.holder.{named_dead}.tmp"),
        format!("{other}.holder.{gone}.tmp"),
        foreign_name.clone(),
        "notes.lock".to_owned(),
    ] {
        std::fs::write(root.join(name), "contents").expect("file");
    }

    let mut claims = HolderClaims::beside(&database);
    claims.claim(session(), fence(1), owner(1)).expect("claim");
    assert!(claims.holds(session()));
    assert_eq!(
        names(directory.path()).len(),
        11,
        "nothing is swept inside the granting transaction"
    );
    claims.sweep_if_pending();

    let mut expected = vec![
        format!("{}.lock", identity()),
        format!("{named_dead}.lock"),
        format!("{live}.lock"),
        format!("{}.holder", stem(branch(7))),
        format!("{}.holder", stem(session())),
        live_temporary,
        foreign_name,
        "notes.lock".to_owned(),
    ];
    expected.sort();
    assert_eq!(names(directory.path()), expected);
    assert!(
        HolderClaims::beside(&database).holder_is_gone(&lease(branch(7), 3, owner(7))),
        "the kept lock still proves its holder gone"
    );
    drop(live_lock);
}

/// A lock file shaped like the earlier format's per-branch lock is not a
/// record of this format and is never read as proof. Builds of that format
/// do not share this directory: it lives in the build's keyed state.
#[test]
fn an_earlier_format_lock_is_not_read_as_proof() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("runtime.sqlite3");
    private_holders(directory.path());
    std::fs::write(
        holders(directory.path()).join(format!("{}.lock", stem(session()))),
        format!("plak-holder-v2 {} 999999 0\nfence 5\n", host()),
    )
    .expect("earlier record");
    assert!(
        !HolderClaims::beside(&database).holder_is_gone(&lease(session(), 5, owner(1))),
        "a free earlier-format lock naming the active fence is not proof"
    );
}

/// Archiving or deleting a session removes its records, and only its own.
#[test]
fn forgetting_a_session_removes_only_its_records() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("runtime.sqlite3");
    let mut claims = HolderClaims::beside(&database);
    claims.claim(branch(0), fence(1), owner(1)).expect("claim");
    claims.claim(branch(1), fence(1), owner(1)).expect("claim");
    claims
        .claim(other_session(), fence(1), owner(1))
        .expect("claim");
    let dead = "999999-00000000000000d1";
    drop(plant(directory.path(), dead, branch(2), 1, owner(2)));

    claims.forget_session(session().session_id());
    assert!(!claims.holds(branch(0)) && !claims.holds(branch(1)));
    assert!(claims.holds(other_session()));
    for gone in [branch(0), branch(1), branch(2)] {
        assert!(!record_path(directory.path(), gone).exists());
    }
    assert!(record_path(directory.path(), other_session()).exists());
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
    let claims = HolderClaims::beside(&database);
    assert!(!claims.holder_is_gone(&lease(session(), 1, owner(1))));
    assert!(
        !holders(directory.path()).exists(),
        "reading a store leaves nothing beside it"
    );
}

/// A new holder directory is private to its user.
#[test]
fn the_holder_directory_is_created_private() {
    use std::os::unix::fs::MetadataExt as _;
    let directory = tempfile::tempdir().expect("temporary directory");
    let mut claims = HolderClaims::beside(&directory.path().join("runtime.sqlite3"));
    claims.claim(session(), fence(1), owner(1)).expect("claim");
    assert!(claims.holds(session()));
    let metadata = std::fs::symlink_metadata(holders(directory.path())).expect("directory");
    assert_eq!(metadata.mode() & 0o777, 0o700);
}

/// A create retried under its operation identity is the same grant — fence
/// and owner — as the attempt that died. If the retry can neither replace nor
/// remove the dead attempt's record, that record would prove the live
/// retry's holder gone, so the claim refuses the grant.
#[test]
fn a_retried_grant_that_cannot_clear_its_dead_twins_record_is_refused() {
    if rustix::process::geteuid().is_root() {
        // Permissions do not bind root, so the record is simply removed.
        return;
    }
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("runtime.sqlite3");
    let dead = "999999-00000000000000be";
    drop(plant(directory.path(), dead, session(), 1, owner(5)));
    std::fs::set_permissions(
        holders(directory.path()),
        std::fs::Permissions::from_mode(0o500),
    )
    .expect("unwritable holder directory");
    let mut retry = HolderClaims::beside(&database);
    let refused = retry.claim(session(), fence(1), owner(5)).is_err();
    let other_grant = retry.claim(session(), fence(1), owner(6)).is_ok();
    std::fs::set_permissions(
        holders(directory.path()),
        std::fs::Permissions::from_mode(0o700),
    )
    .expect("writable holder directory");
    assert!(refused, "the dead twin's record names this grant");
    assert!(
        other_grant,
        "a record of another grant is no reason to refuse"
    );
    assert!(
        HolderClaims::beside(&database).holder_is_gone(&lease(session(), 1, owner(5))),
        "had the grant committed, the dead twin's record would have proved it gone"
    );
}

/// A holder directory replaced under a live process is locked again before
/// anything is published into it.
///
/// Otherwise a store would keep the old directory's lock while publishing
/// into the new one, where another store of this process may have taken and
/// then released this identity's lock: the live store's record would name a
/// free lock.
#[test]
fn a_holder_directory_replaced_under_a_live_store_is_locked_again() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("runtime.sqlite3");
    let mut live = HolderClaims::beside(&database);
    live.claim(branch(0), fence(1), owner(1)).expect("claim");

    std::fs::rename(
        holders(directory.path()),
        directory.path().join("old-holders"),
    )
    .expect("move the directory away");
    let mut passing = HolderClaims::beside(&database);
    passing.claim(branch(1), fence(1), owner(2)).expect("claim");
    assert!(passing.holds(branch(1)));
    drop(passing);
    assert!(
        !is_locked(&lock_path(directory.path(), identity())),
        "the passing store's lock in the new directory is free"
    );

    live.claim(branch(2), fence(1), owner(1)).expect("claim");
    assert!(live.holds(branch(2)));
    assert!(
        record_path(directory.path(), branch(2)).exists(),
        "published into the directory as it is now"
    );
    assert!(
        is_locked(&lock_path(directory.path(), identity())),
        "the record names a lock the live store holds"
    );
}

/// A lock file removed from under a live store is taken again before the next
/// record names it.
#[test]
fn a_lock_file_removed_under_a_live_store_is_taken_again() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("runtime.sqlite3");
    let mut live = HolderClaims::beside(&database);
    live.claim(branch(0), fence(1), owner(1)).expect("claim");
    std::fs::remove_file(lock_path(directory.path(), identity())).expect("remove lock");
    live.claim(branch(1), fence(1), owner(1)).expect("claim");
    assert!(live.holds(branch(1)));
    assert!(is_locked(&lock_path(directory.path(), identity())));
}

/// Release removes the record only when it names this process and the whole
/// claimed grant: the same identity and owner under another fence, or the
/// same fence under another owner, is another grant's record.
#[test]
fn release_leaves_a_record_of_another_grant() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("runtime.sqlite3");
    let path = record_path(directory.path(), session());
    let mut claims = HolderClaims::beside(&database);
    for (fence_value, owner_value) in [(2, 1), (1, 2)] {
        claims.claim(session(), fence(1), owner(1)).expect("claim");
        let other = local_record(identity(), fence_value, owner(owner_value));
        std::fs::write(&path, &other).expect("another grant's record");
        claims.release(session());
        assert!(!claims.holds(session()));
        assert_eq!(
            std::fs::read_to_string(&path).expect("record kept"),
            other,
            "another grant's record is kept"
        );
    }
}

/// A lock path unlinked and recreated between this process's open and its
/// lock names a file the lock is not on; the claim locks the file the path
/// names.
#[test]
fn a_lock_file_replaced_before_it_is_locked_is_taken_again() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("runtime.sqlite3");
    BEFORE_LOCK.with(|hook| {
        hook.set(Some(|path| {
            std::fs::remove_file(path).expect("unlink lock");
            drop(File::create(path).expect("recreate lock"));
        }));
    });
    let mut claims = HolderClaims::beside(&database);
    claims.claim(session(), fence(1), owner(1)).expect("claim");
    assert!(claims.holds(session()));
    assert!(
        is_locked(&lock_path(directory.path(), identity())),
        "the lock the record names is held"
    );
}

/// Neither a symbolic link at a record's path nor one at a lock's path is
/// followed, so neither is proof.
#[test]
fn symbolic_links_are_not_proof() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("runtime.sqlite3");
    let claims = HolderClaims::beside(&database);
    let active = lease(session(), 1, owner(1));
    let dead = "999999-00000000000000bf";
    drop(plant(directory.path(), dead, session(), 1, owner(1)));
    assert!(claims.holder_is_gone(&active), "the planted claim is proof");

    let elsewhere = directory.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("elsewhere");
    std::fs::rename(
        lock_path(directory.path(), dead),
        elsewhere.join("free.lock"),
    )
    .expect("move lock");
    std::os::unix::fs::symlink(
        elsewhere.join("free.lock"),
        lock_path(directory.path(), dead),
    )
    .expect("symlinked lock");
    assert!(
        !claims.holder_is_gone(&active),
        "a symlinked lock is not proof"
    );

    std::fs::remove_file(lock_path(directory.path(), dead)).expect("remove link");
    drop(File::create(lock_path(directory.path(), dead)).expect("lock"));
    std::fs::rename(
        record_path(directory.path(), session()),
        elsewhere.join("record"),
    )
    .expect("move record");
    std::os::unix::fs::symlink(
        elsewhere.join("record"),
        record_path(directory.path(), session()),
    )
    .expect("symlinked record");
    assert!(
        !claims.holder_is_gone(&active),
        "a symlinked record is not proof"
    );
}

const CHILD_DATABASE: &str = "PLAKOUS_STORE_HOLDER_CHILD_DATABASE";
const CHILD_FIRST_BRANCH: &str = "PLAKOUS_STORE_HOLDER_CHILD_FIRST_BRANCH";
const CONCURRENT: &str =
    "holder::tests::two_processes_claim_concurrently_and_each_proves_only_its_own_death";
const BRANCHES_PER_CHILD: u128 = 64;
const SHARED_BRANCH: u128 = 1_000;
const SHARED_ROUNDS: u64 = 200;

/// Kills its child when dropped, so a failed assertion leaves no orphan.
struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Child {
    fn spawn(database: &Path, first_branch: u128) -> Self {
        let child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", CONCURRENT, "--nocapture"])
            .env(CHILD_DATABASE, database)
            .env(CHILD_FIRST_BRANCH, first_branch.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("child");
        Self(child)
    }

    fn wait_for_claims(&mut self) {
        use std::io::Read as _;
        let mut out = self.0.stdout.take().expect("child stdout");
        let mut seen = String::new();
        let mut byte = [0_u8; 1];
        while !seen.contains("held") {
            assert_ne!(
                out.read(&mut byte).expect("child output"),
                0,
                "child exited"
            );
            seen.push(char::from(byte[0]));
        }
    }

    fn kill(mut self) {
        self.0.kill().expect("kill child");
        self.0.wait().expect("reap child");
    }
}

/// The owner a child's grants are recorded under.
fn child_owner(first_branch: u128) -> RunId {
    owner(0x100 + first_branch)
}

/// The child half: claims its own branches, then races the other child on a
/// shared branch with fences of its own parity, then holds everything.
fn concurrent_child(database: &Path, first_branch: u128) {
    let mut claims = HolderClaims::beside(database);
    for n in first_branch..first_branch + BRANCHES_PER_CHILD {
        claims
            .claim(branch(n), fence(1), child_owner(first_branch))
            .expect("claim");
        assert!(claims.holds(branch(n)), "child claim {n}");
    }
    claims.sweep_if_pending();
    let parity = u64::from(first_branch != 0);
    for round in 0..SHARED_ROUNDS {
        claims
            .claim(
                branch(SHARED_BRANCH),
                fence(2 * round + 1 + parity),
                child_owner(first_branch),
            )
            .expect("claim");
    }
    println!("held");
    std::thread::sleep(Duration::from_secs(600));
}

/// Two processes claim branches in one directory at once, each under its own
/// lock, and each one's death proves only its own branches gone.
///
/// Both also replace the record of one shared branch hundreds of times while
/// this process reads it: every record read is a whole one, never a torn
/// write that could name the wrong grant.
#[test]
fn two_processes_claim_concurrently_and_each_proves_only_its_own_death() {
    if let Some(database) = std::env::var_os(CHILD_DATABASE) {
        let first = std::env::var(CHILD_FIRST_BRANCH)
            .expect("first branch")
            .parse()
            .expect("first branch number");
        concurrent_child(Path::new(&database), first);
        return;
    }
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("runtime.sqlite3");
    let mut children = [Child::spawn(&database, 0), Child::spawn(&database, 100)];

    let shared = record_path(directory.path(), branch(SHARED_BRANCH));
    // A liveness bound only: generous, so load never trips it.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut reads = 0_usize;
    while Instant::now() < deadline && reads < 2_000 {
        if let Ok(recorded) = std::fs::read_to_string(&shared) {
            assert!(
                super::parse_record(&recorded).is_some(),
                "a whole record, never a torn one: {recorded:?}"
            );
            reads += 1;
        }
    }
    for child in &mut children {
        child.wait_for_claims();
    }
    let [first, second] = children;

    let first_grants = || (0..BRANCHES_PER_CHILD).map(|n| lease(branch(n), 1, child_owner(0)));
    let second_grants =
        || (100..100 + BRANCHES_PER_CHILD).map(|n| lease(branch(n), 1, child_owner(100)));
    let locks = names(directory.path())
        .into_iter()
        .filter(|name| name.strip_suffix(".lock").is_some())
        .count();
    assert_eq!(locks, 2, "one lock per process");
    let holder = |session| {
        super::read_record(&record_path(directory.path(), session))
            .expect("record")
            .identity
    };
    let (first_holder, second_holder) = (holder(branch(0)), holder(branch(100)));
    assert_ne!(first_holder, second_holder);
    assert!(first_grants().all(|grant| holder(grant.session()) == first_holder));
    assert!(second_grants().all(|grant| holder(grant.session()) == second_holder));

    let claims = HolderClaims::beside(&database);
    assert!(
        first_grants()
            .chain(second_grants())
            .all(|grant| !claims.holder_is_gone(&grant)),
        "live holders are not gone"
    );
    first.kill();
    assert!(
        first_grants().all(|grant| claims.holder_is_gone(&grant)),
        "the dead process's branches are proven gone"
    );
    assert!(
        second_grants().all(|grant| !claims.holder_is_gone(&grant)),
        "the live process's branches are not"
    );
    second.kill();
    assert!(second_grants().all(|grant| claims.holder_is_gone(&grant)));
}

/// A host's directory names a branch's holder only while it is not this
/// process and not proven gone: a live holder on this machine (its lock
/// held) and one on another machine are named with their process id; no
/// record, this process's own record, a free lock and a missing one name
/// nobody.
#[test]
fn branch_holder_names_only_a_holder_not_proven_gone() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("runtime.sqlite3");
    let (session_id, branch_id) = (
        session().session_id().to_string(),
        session().branch_id().to_string(),
    );
    let holder = || super::branch_holder(&database, &session_id, &branch_id);
    assert_eq!(holder(), None, "no record");
    let live = "424242-00000000000000c1";
    let held = plant(directory.path(), live, session(), 2, owner(1));
    lock(&held);
    assert_eq!(
        holder(),
        Some(super::BranchHolder {
            host_name: host(),
            pid: 424_242
        })
    );
    // A holder directory others could write names nobody.
    std::fs::set_permissions(
        holders(directory.path()),
        std::fs::Permissions::from_mode(0o770),
    )
    .expect("group-writable holder directory");
    assert_eq!(holder(), None, "a directory others can write was read");
    private_holders(directory.path());
    drop(held);
    assert_eq!(holder(), None, "a free lock is a gone holder");
    std::fs::remove_file(lock_path(directory.path(), live)).expect("remove lock");
    assert_eq!(holder(), None, "a missing lock names no live holder");
    std::fs::write(
        record_path(directory.path(), session()),
        format!(
            "{FORMAT} another-machine {live}\nfence 2\nowner {}\n",
            owner(1)
        ),
    )
    .expect("remote record");
    assert_eq!(
        holder().map(|holder| holder.host_name),
        Some("another-machine".to_owned()),
        "a holder elsewhere is never proven gone"
    );
    std::fs::write(
        record_path(directory.path(), session()),
        local_record(identity(), 2, owner(1)),
    )
    .expect("own record");
    assert_eq!(holder(), None, "this process is not elsewhere");
}

/// Probes of one dead holder's free lock from two threads of this process
/// never read it alive: each probe is serialized, so neither sees the
/// other's momentary lock.
#[test]
fn concurrent_probes_never_read_a_dead_holder_alive() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("runtime.sqlite3");
    let dead = "999999-00000000000000c2";
    drop(plant(directory.path(), dead, session(), 2, owner(1)));
    let (session_id, branch_id) = (
        session().session_id().to_string(),
        session().branch_id().to_string(),
    );
    let named = std::thread::scope(|scope| {
        let probes: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    (0..2_000)
                        .filter(|_| {
                            super::branch_holder(&database, &session_id, &branch_id).is_some()
                        })
                        .count()
                })
            })
            .collect();
        probes
            .into_iter()
            .map(|probe| probe.join().expect("probe"))
            .sum::<usize>()
    });
    assert_eq!(named, 0, "a dead holder was read alive");
}

/// The gone-holder proof waits for any other probe of this process: it
/// takes the lock only while no other probe holds one.
#[test]
fn the_gone_holder_proof_waits_for_another_probe() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("runtime.sqlite3");
    let dead = "999999-00000000000000c3";
    drop(plant(directory.path(), dead, session(), 2, owner(1)));
    let probing = super::PROBES.lock().expect("probes");
    let (done, proved) = std::sync::mpsc::channel();
    let prober = std::thread::spawn(move || {
        let claims = HolderClaims::beside(&database);
        let _ = done.send(claims.holder_is_gone(&lease(session(), 2, owner(1))));
    });
    assert!(
        proved.recv_timeout(Duration::from_millis(200)).is_err(),
        "the proof probed beside another probe"
    );
    drop(probing);
    assert_eq!(proved.recv_timeout(Duration::from_secs(10)), Ok(true));
    prober.join().expect("prober");
}
