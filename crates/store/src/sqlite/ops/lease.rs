//! Branch lease acquire, renew, revive, and release.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the sqlite module that shares its vocabulary"
)]

use super::super::*;

pub(in super::super) fn acquire(
    b: &mut Backend,
    r: AcquireBranch,
) -> Result<BranchLease, SessionStoreError> {
    b.require_open()?;
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    let now = sample_clock(b.clock.as_ref(), b.domain)?;
    live_session(&tx, r.session().session_id())?;
    let row = branch_row(&tx, r.session())?.ok_or(SessionStoreError::NotFound)?;
    if let Some(current) = row.active.as_ref()
        && !expired(current, now)?
    {
        if current.owner() == r.owner() && current.lease_seconds() == r.lease_seconds() {
            let out = current.clone();
            tx.commit().map_err(map_sqlite)?;
            return Ok(out);
        }
        // A lease exists to stop two live processes sharing a branch. When the
        // holder's process is provably gone, waiting out its clock protects
        // nothing and reads to the next user as a session that will not open.
        // Anything short of proof keeps the clock. Probed inside the writer's
        // transaction, which serializes it against every other reclaim.
        if !b.holders.holder_is_gone(current) {
            tx.commit().map_err(map_sqlite)?;
            return Err(SessionStoreError::LeaseHeld);
        }
    }
    let fence = NonZeroU64::new(row.last_fence.checked_add(1).ok_or_else(|| {
        SessionStoreError::capacity(StoreCapacityBound::LeaseFence, row.last_fence, 0)
    })?)
    .ok_or(SessionStoreError::corrupt())?;
    let value = BranchLease::from_store(
        r.session(),
        r.owner(),
        fence,
        expiry(now, r.lease_seconds())?,
        r.lease_seconds(),
    );
    write_active(&tx, &value, None)?;
    bump(&tx)?;
    commit_claimed(tx, &mut b.holders, r.session(), fence, r.owner())?;
    Ok(value)
}

/// Commits a transaction that grants `session` a lease under `fence` to
/// `owner`, holding the branch's host-local claim from before the commit.
///
/// Claimed first, so the record and the lease become visible together: a
/// holder that dies the instant its grant commits is already provably gone.
/// A grant that fails to commit gives its claim back. A claim refused because
/// a dead attempt's record of this same grant cannot be removed rolls the
/// grant back: committed, it would be provably gone the moment it began. The
/// first claim of a process sweeps the holder directory, after the
/// transaction.
pub(in super::super) fn commit_claimed(
    tx: Transaction<'_>,
    holders: &mut crate::holder::HolderClaims,
    session: SessionRef,
    fence: NonZeroU64,
    owner: RunId,
) -> Result<(), SessionStoreError> {
    let committed = if holders.claim(session, fence, owner).is_ok() {
        tx.commit().map_err(|error| {
            holders.release(session);
            map_sqlite(error)
        })
    } else {
        drop(tx);
        Err(SessionStoreError::BackendUnavailable)
    };
    holders.sweep_if_pending();
    committed
}

pub(in super::super) fn renew(
    b: &mut Backend,
    l: &BranchLease,
) -> Result<BranchLease, SessionStoreError> {
    b.require_open()?;
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    let now = sample_clock(b.clock.as_ref(), b.domain)?;
    live_session(&tx, l.session().session_id())?;
    let row = branch_row(&tx, l.session())?.ok_or(SessionStoreError::NotFound)?;
    forfeit_if_superseded(&mut b.holders, &row, l);
    require_lease_domain(l, now)?;
    if row.previous.as_ref() == Some(l) {
        let active = row.active.ok_or(SessionStoreError::corrupt())?;
        require_lease_domain(&active, now)?;
        let out = if expired(&active, now)? {
            Err(SessionStoreError::LeaseExpired)
        } else {
            Ok(active)
        };
        tx.commit().map_err(map_sqlite)?;
        return out;
    }
    require_lease(&row, l, now)?;
    let expires_at = expiry(now, l.lease_seconds())?;
    if expires_at.same_domain_cmp(l.expires_at())? != std::cmp::Ordering::Greater {
        tx.commit().map_err(map_sqlite)?;
        return Ok(l.clone());
    }
    let value = BranchLease::from_store(
        l.session(),
        l.owner(),
        l.fence(),
        expires_at,
        l.lease_seconds(),
    );
    write_active(&tx, &value, Some(l))?;
    bump(&tx)?;
    tx.commit().map_err(map_sqlite)?;
    Ok(value)
}

/// Extends an expired lease under its own fence when nothing has happened to
/// the branch since: it still names exactly this lease, no acquire has bumped
/// the fence, this store still holds the host claim it took with the lease, and
/// the clock domain is unchanged. Any acquire replaces `active` and bumps
/// `last_fence` first, so a revival and an acquire cannot both succeed.
pub(in super::super) fn revive(
    b: &mut Backend,
    l: &BranchLease,
) -> Result<BranchLease, SessionStoreError> {
    b.require_open()?;
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    let now = sample_clock(b.clock.as_ref(), b.domain)?;
    live_session(&tx, l.session().session_id())?;
    let row = branch_row(&tx, l.session())?.ok_or(SessionStoreError::NotFound)?;
    forfeit_if_superseded(&mut b.holders, &row, l);
    require_lease_domain(l, now)?;
    if row.active.as_ref() != Some(l) || row.last_fence != l.fence().get() {
        tx.commit().map_err(map_sqlite)?;
        return Err(SessionStoreError::FenceSuperseded);
    }
    if !expired(l, now)? {
        drop(tx);
        return renew(b, l);
    }
    if !b.holders.holds(l.session()) {
        tx.commit().map_err(map_sqlite)?;
        return Err(SessionStoreError::FenceSuperseded);
    }
    let value = BranchLease::from_store(
        l.session(),
        l.owner(),
        l.fence(),
        expiry(now, l.lease_seconds())?,
        l.lease_seconds(),
    );
    write_active(&tx, &value, Some(l))?;
    bump(&tx)?;
    tx.commit().map_err(map_sqlite)?;
    Ok(value)
}

pub(in super::super) fn release_many(
    b: &mut Backend,
    leases: &[BranchLease],
) -> Result<(), SessionStoreError> {
    b.require_open()?;
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    let now = sample_clock(b.clock.as_ref(), b.domain)?;
    let mut changed = false;
    for lease in leases {
        live_session(&tx, lease.session().session_id())?;
        let row = branch_row(&tx, lease.session())?.ok_or(SessionStoreError::NotFound)?;
        forfeit_if_superseded(&mut b.holders, &row, lease);
        require_lease_domain(lease, now)?;
        if row.released.as_ref() == Some(lease) && row.active.is_none() {
            continue;
        }
        require_current_lease(&row, lease, now)?;
        mark_released(&tx, lease)?;
        feed::append(
            &tx,
            lease.session(),
            FeedChange::LeaseReleased {
                head: row.head.checkpoint().checkpoint_id(),
                fence: lease.fence().get(),
            },
            &[],
        )?;
        changed = true;
    }
    if changed {
        bump(&tx)?;
    }
    tx.commit().map_err(map_sqlite)?;
    for lease in leases {
        b.holders.release(lease.session());
    }
    let sessions = leases
        .iter()
        .map(|lease| lease.session().session_id())
        .collect::<BTreeSet<_>>();
    for session_id in sessions {
        feed::trim(b, session_id);
    }
    Ok(())
}

/// Drops this store's claim for `lease` once another acquire has replaced it,
/// so a superseded grant is never revived as one this store still holds.
fn forfeit_if_superseded(
    holders: &mut crate::holder::HolderClaims,
    row: &BranchRow,
    lease: &BranchLease,
) {
    if row.last_fence != lease.fence().get() {
        holders.forfeit(lease.session(), lease.fence());
    }
}

fn mark_released(tx: &Transaction<'_>, l: &BranchLease) -> Result<(), SessionStoreError> {
    tx.execute(
        "UPDATE branches SET active_owner=NULL,active_fence=NULL,active_time=NULL,active_ns=NULL,
        active_duration=NULL,previous_time=NULL,previous_ns=NULL,released_owner=?3,released_fence=?4,
        released_time=?5,released_ns=?6,released_duration=?7 WHERE session_id=?1 AND branch_id=?2",
        params![id(l.session().session_id()), id(l.session().branch_id()), id(l.owner()), u64b(l.fence().get()), lease_time_bytes(l.expires_at()), i64::from(l.expires_at().nanoseconds()), u64b(l.lease_seconds().get())],
    ).map_err(map_sqlite)?;
    Ok(())
}

pub(in super::super) fn release(b: &mut Backend, l: &BranchLease) -> Result<(), SessionStoreError> {
    b.require_open()?;
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    let now = sample_clock(b.clock.as_ref(), b.domain)?;
    live_session(&tx, l.session().session_id())?;
    let row = branch_row(&tx, l.session())?.ok_or(SessionStoreError::NotFound)?;
    forfeit_if_superseded(&mut b.holders, &row, l);
    require_lease_domain(l, now)?;
    if row.released.as_ref() == Some(l) && row.active.is_none() {
        tx.commit().map_err(map_sqlite)?;
        b.holders.release(l.session());
        return Ok(());
    }
    require_current_lease(&row, l, now)?;
    mark_released(&tx, l)?;
    feed::append(
        &tx,
        l.session(),
        FeedChange::LeaseReleased {
            head: row.head.checkpoint().checkpoint_id(),
            fence: l.fence().get(),
        },
        &[],
    )?;
    bump(&tx)?;
    tx.commit().map_err(map_sqlite)?;
    // The record and the lease end together, so a released branch is not left
    // looking like one whose holder died.
    b.holders.release(l.session());
    Ok(())
}

pub(in super::super) fn write_active(
    tx: &Transaction<'_>,
    l: &BranchLease,
    previous: Option<&BranchLease>,
) -> Result<(), SessionStoreError> {
    tx.execute(
        "UPDATE branches SET last_fence=?3,active_owner=?4,active_fence=?3,active_time=?5,
        active_ns=?6,active_duration=?7,previous_time=?8,previous_ns=?9,released_owner=NULL,
        released_fence=NULL,released_time=NULL,released_ns=NULL,released_duration=NULL
        WHERE session_id=?1 AND branch_id=?2",
        params![
            id(l.session().session_id()),
            id(l.session().branch_id()),
            u64b(l.fence().get()),
            id(l.owner()),
            lease_time_bytes(l.expires_at()),
            i64::from(l.expires_at().nanoseconds()),
            u64b(l.lease_seconds().get()),
            previous.map(|x| lease_time_bytes(x.expires_at())),
            previous.map(|x| i64::from(x.expires_at().nanoseconds()))
        ],
    )
    .map_err(map_sqlite)?;
    Ok(())
}

pub(in super::super) fn require_current_lease(
    row: &BranchRow,
    l: &BranchLease,
    now: LeaseInstant,
) -> Result<(), SessionStoreError> {
    require_lease_domain(l, now)?;
    let active = row
        .active
        .as_ref()
        .ok_or(SessionStoreError::FenceSuperseded)?;
    if active != l {
        return Err(SessionStoreError::FenceSuperseded);
    }
    Ok(())
}
