//! Branch rows, session heads, and horizon reads.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the sqlite module that shares its vocabulary"
)]

use super::*;

#[derive(Clone)]
pub(super) struct BranchRow {
    pub(super) head: BranchHead,
    pub(super) last_fence: u64,
    pub(super) lineage: Option<ForkLineage>,
    pub(super) active: Option<BranchLease>,
    pub(super) previous: Option<BranchLease>,
    pub(super) released: Option<BranchLease>,
}

pub(super) fn branch_row(
    c: &Connection,
    s: SessionRef,
) -> Result<Option<BranchRow>, SessionStoreError> {
    branch_row_with_checkpoint(c, s).map(|row| row.map(|(branch, _)| branch))
}

pub(super) fn branch_row_with_checkpoint(
    c: &Connection,
    s: SessionRef,
) -> Result<Option<(BranchRow, CheckpointRow)>, SessionStoreError> {
    c.query_row("SELECT revision,checkpoint_id,last_fence,fork_session,fork_branch,fork_checkpoint,
        fork_revision,active_owner,active_fence,active_time,active_ns,active_duration,
        previous_time,previous_ns,released_owner,released_fence,released_time,released_ns,released_duration
        FROM branches WHERE session_id=?1 AND branch_id=?2",params![id(s.session_id()),id(s.branch_id())],|r|Ok((
        r.get::<_,Vec<u8>>(0)?,r.get::<_,String>(1)?,r.get::<_,Vec<u8>>(2)?,
        r.get::<_,Option<String>>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,Option<String>>(5)?,
        r.get::<_,Option<Vec<u8>>>(6)?,r.get::<_,Option<String>>(7)?,r.get::<_,Option<Vec<u8>>>(8)?,
        r.get::<_,Option<Vec<u8>>>(9)?,r.get::<_,Option<i64>>(10)?,r.get::<_,Option<Vec<u8>>>(11)?,
        r.get::<_,Option<Vec<u8>>>(12)?,r.get::<_,Option<i64>>(13)?,r.get::<_,Option<String>>(14)?,
        r.get::<_,Option<Vec<u8>>>(15)?,r.get::<_,Option<Vec<u8>>>(16)?,
        r.get::<_,Option<i64>>(17)?,r.get::<_,Option<Vec<u8>>>(18)?)))
    .optional().map_err(map_sqlite)?.map(|v|{
        let cp=checkpoint_row(c,parse(&v.1)?)?.ok_or(SessionStoreError::corrupt())?;
        let active=lease(s,v.7.as_deref(),v.8.as_deref(),v.9.as_deref(),v.10,v.11.as_deref())?;
        let previous=match(&v.12,v.13,&active){(None,None,_)=>None,(Some(sec),Some(ns),Some(a))=>
            Some(BranchLease::from_store(s,a.owner(),a.fence(),lease_time(sec,ns)?,a.lease_seconds())),
            _=>return Err(SessionStoreError::corrupt())};
        let released=lease(s,v.14.as_deref(),v.15.as_deref(),v.16.as_deref(),v.17,v.18.as_deref())?;
        if active.is_some()&&released.is_some(){return Err(SessionStoreError::corrupt());}
        Ok((BranchRow{head:BranchHead::from_store(s,BranchRevision::from_store(read_u64(&v.0)?),cp.reference),
            last_fence:read_u64(&v.2)?,lineage:lineage(v.3.as_deref(),v.4.as_deref(),v.5.as_deref(),v.6.as_deref())?,
            active,previous,released}, cp))
    }).transpose()
}

pub(super) fn insert_branch(
    tx: &Transaction<'_>,
    h: BranchHead,
    source: Option<BranchHead>,
    fence: u64,
    active: Option<&BranchLease>,
    line: (plakous_agent::internal::LineId, u64),
) -> Result<(), SessionStoreError> {
    let l = source.map(|x| ForkLineage {
        session_id: x.session().session_id(),
        branch_id: x.session().branch_id(),
        checkpoint_id: x.checkpoint().checkpoint_id(),
        branch_revision: x.revision().get(),
    });
    tx.execute(
        "INSERT INTO branches(session_id,branch_id,revision,checkpoint_id,last_fence,
        fork_session,fork_branch,fork_checkpoint,fork_revision,active_owner,active_fence,
        active_time,active_ns,active_duration,line_id,line_generation)
        VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
        params![
            id(h.session().session_id()),
            id(h.session().branch_id()),
            u64b(h.revision().get()),
            id(h.checkpoint().checkpoint_id()),
            u64b(fence),
            l.map(|x| id(x.session_id)),
            l.map(|x| id(x.branch_id)),
            l.map(|x| id(x.checkpoint_id)),
            l.map(|x| u64b(x.branch_revision)),
            active.map(|x| id(x.owner())),
            active.map(|x| u64b(x.fence().get())),
            active.map(|x| lease_time_bytes(x.expires_at())),
            active.map(|x| i64::from(x.expires_at().nanoseconds())),
            active.map(|x| u64b(x.lease_seconds().get())),
            id(line.0),
            u64b(line.1)
        ],
    )
    .map_err(map_sqlite)?;
    Ok(())
}

pub(super) fn session_head(
    c: &Connection,
    sid: SessionId,
) -> Result<Option<SessionHead>, SessionStoreError> {
    c.query_row("SELECT initial_branch,revision,created_s,created_ns,durability,archived FROM sessions WHERE session_id=?1",
        [id(sid)],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Vec<u8>>(1)?,r.get::<_,Vec<u8>>(2)?,
        r.get::<_,i64>(3)?,r.get::<_,i64>(4)?,r.get::<_,i64>(5)?))).optional().map_err(map_sqlite)?
        .map(|v|Ok(SessionHead::from_store(sid,parse(&v.0)?,SessionRevision::from_store(read_u64(&v.1)?),
            timestamp(&v.2,v.3)?,decode_durability(v.4)?,boolv(v.5)?))).transpose()
}

pub(super) fn live_session(c: &Connection, s: SessionId) -> Result<SessionHead, SessionStoreError> {
    let h = session_head(c, s)?.ok_or(SessionStoreError::NotFound)?;
    if h.is_archived() {
        Err(SessionStoreError::Archived)
    } else {
        Ok(h)
    }
}

pub(super) fn require_lease(
    row: &BranchRow,
    l: &BranchLease,
    now: LeaseInstant,
) -> Result<(), SessionStoreError> {
    ops::require_current_lease(row, l, now)?;
    if expired(l, now)? {
        return Err(SessionStoreError::LeaseExpired);
    }
    Ok(())
}

pub(super) fn require_unused_session_identity(
    c: &Connection,
    session_id: SessionId,
) -> Result<(), SessionStoreError> {
    if session_head(c, session_id)?.is_some() || ops::deletion_exists(c, session_id)? {
        return Err(SessionStoreError::IdentityConflict);
    }
    Ok(())
}

/// Queues roots in the same transaction that removes their checkpoint authority.
pub(super) fn capture_released_roots(
    tx: &Transaction<'_>,
    condition: &str,
    parameter: &str,
) -> Result<(), SessionStoreError> {
    tx.execute(&format!("INSERT OR IGNORE INTO gc_candidates SELECT root_kind,root_codec,root_digest FROM checkpoints WHERE {condition}"), [parameter]).map_err(map_sqlite)?;
    Ok(())
}

/// One branch's recorded history horizon and cumulative reclaim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct BranchHorizon {
    pub(super) selector: SessionRef,
    pub(super) revision: BranchRevision,
    pub(super) checkpoint: CheckpointId,
    pub(super) pruned_checkpoints: u64,
}

pub(super) fn horizon_integrity(h: &BranchHorizon) -> Result<Vec<u8>, SessionStoreError> {
    receipt_integrity(
        b"branch-horizon-v1",
        &[
            session_ref_bytes(h.selector),
            h.revision.get().to_be_bytes().to_vec(),
            text_bytes(h.checkpoint),
            h.pruned_checkpoints.to_be_bytes().to_vec(),
        ],
    )
}

pub(super) fn decode_horizon(
    selector: SessionRef,
    revision: &[u8],
    checkpoint: &str,
    count: i64,
    integrity: &[u8],
) -> Result<BranchHorizon, SessionStoreError> {
    let horizon = BranchHorizon {
        selector,
        revision: BranchRevision::from_store(read_u64(revision)?),
        checkpoint: parse(checkpoint)?,
        pruned_checkpoints: nonnegative(count)?,
    };
    if horizon_integrity(&horizon)? != integrity {
        return Err(SessionStoreError::corrupt());
    }
    Ok(horizon)
}

pub(super) fn load_horizon(
    c: &Connection,
    selector: SessionRef,
) -> Result<Option<BranchHorizon>, SessionStoreError> {
    c.query_row(
        "SELECT horizon_revision,horizon_checkpoint,pruned_checkpoints,integrity FROM branch_horizons
         WHERE session_id=?1 AND branch_id=?2",
        params![id(selector.session_id()), id(selector.branch_id())],
        |z| {
            Ok((
                z.get::<_, Vec<u8>>(0)?,
                z.get::<_, String>(1)?,
                z.get::<_, i64>(2)?,
                z.get::<_, Vec<u8>>(3)?,
            ))
        },
    )
    .optional()
    .map_err(map_sqlite)?
    .map(|v| decode_horizon(selector, &v.0, &v.1, v.2, &v.3))
    .transpose()
}

pub(super) fn pruned_checkpoint(
    c: &Connection,
    checkpoint: CheckpointId,
) -> Result<bool, SessionStoreError> {
    c.query_row(
        "SELECT 1 FROM pruned_checkpoints WHERE checkpoint_id=?1",
        [id(checkpoint)],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
    .map_err(map_sqlite)
}

/// Whether retention reclaimed this checkpoint's commit receipt: the
/// checkpoint itself was pruned, or it is a retained checkpoint at or below
/// its branch horizon.
pub(super) fn commit_history_pruned(
    c: &Connection,
    checkpoint: CheckpointId,
) -> Result<bool, SessionStoreError> {
    if pruned_checkpoint(c, checkpoint)? {
        return Ok(true);
    }
    c.query_row(
        "SELECT 1 FROM checkpoints k JOIN branch_horizons h
         ON h.session_id=k.session_id AND h.branch_id=k.branch_id
         WHERE k.checkpoint_id=?1 AND k.revision<>zeroblob(8) AND k.revision<=h.horizon_revision",
        [id(checkpoint)],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
    .map_err(map_sqlite)
}
