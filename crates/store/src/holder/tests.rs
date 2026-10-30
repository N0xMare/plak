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
