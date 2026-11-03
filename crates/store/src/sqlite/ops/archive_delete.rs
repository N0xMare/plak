//! Archive and delete transactions.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the sqlite module that shares its vocabulary"
)]

use super::super::*;

pub(in super::super) fn archive(
    b: &mut Backend,
    r: ArchiveSession,
) -> Result<ArchiveReceipt, SessionStoreError> {
    b.require_open()?;
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    let now = sample_clock(b.clock.as_ref(), b.domain)?;
    if let Some(old) = load_archive(&tx, r.session_id())? {
        let out = if old.request() == r {
            Ok(old)
        } else {
            Err(SessionStoreError::Archived)
        };
        tx.commit().map_err(map_sqlite)?;
        return out;
    }
    let session = session_head(&tx, r.session_id())?.ok_or(SessionStoreError::NotFound)?;
    if session.is_archived() {
        return Err(SessionStoreError::Archived);
    }
    if session.revision() != r.expected_revision() {
        return Err(SessionStoreError::Conflict);
    }
    let mut s = tx
        .prepare("SELECT branch_id FROM branches WHERE session_id=?1")
        .map_err(map_sqlite)?;
    let ids = s
        .query_map([id(r.session_id())], |z| z.get::<_, String>(0))
        .map_err(map_sqlite)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sqlite)?;
    drop(s);
    for value in ids {
        let row = branch_row(&tx, SessionRef::new(r.session_id(), parse(&value)?))?
            .ok_or(SessionStoreError::corrupt())?;
        if row.active.as_ref().map(|l| expired(l, now)).transpose()? == Some(false) {
            return Err(SessionStoreError::LeaseHeld);
        }
    }
    let revision = r.expected_revision().checked_next().ok_or_else(|| {
        SessionStoreError::capacity(
            StoreCapacityBound::SessionRevision,
            r.expected_revision().get(),
            0,
        )
    })?;
    let archived = SessionHead::from_store(
        session.session_id(),
        session.initial_branch(),
        revision,
        session.created_at(),
        session.durability(),
        true,
    );
    tx.execute(
        "UPDATE sessions SET revision=?2,archived=1 WHERE session_id=?1",
        params![id(r.session_id()), u64b(revision.get())],
    )
    .map_err(map_sqlite)?;
    tx.execute(
        "UPDATE branches SET active_owner=NULL,active_fence=NULL,active_time=NULL,active_ns=NULL,
        active_duration=NULL,previous_time=NULL,previous_ns=NULL WHERE session_id=?1",
        [id(r.session_id())],
    )
    .map_err(map_sqlite)?;
    feed::append(
        &tx,
        SessionRef::new(r.session_id(), session.initial_branch()),
        FeedChange::Archived { revision },
        &[],
    )?;
    let receipt = ArchiveReceipt::from_store(r, session, archived);
    let receipt_integrity = archive_receipt_integrity(receipt)?;
    exactly_one(
        tx.execute(
            "INSERT INTO archive_receipts VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                id(session.session_id()),
                id(session.initial_branch()),
                u64b(session.created_at().seconds()),
                i64::from(session.created_at().nanoseconds()),
                durability_tag(session.durability())?,
                u64b(session.revision().get()),
                u64b(revision.get()),
                receipt_integrity
            ],
        )
        .map_err(map_sqlite)?,
    )?;
    bump(&tx)?;
    tx.commit().map_err(map_sqlite)?;
    Ok(receipt)
}

fn load_archive(
    c: &Connection,
    sid: SessionId,
) -> Result<Option<ArchiveReceipt>, SessionStoreError> {
    c.query_row(
        "SELECT initial_branch,created_s,created_ns,durability,expected_revision,
        session_revision,receipt_integrity FROM archive_receipts WHERE session_id=?1",
        [id(sid)],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Vec<u8>>(4)?,
                r.get::<_, Vec<u8>>(5)?,
                r.get::<_, Vec<u8>>(6)?,
            ))
        },
    )
    .optional()
    .map_err(map_sqlite)?
    .map(|v| {
        let initial = parse(&v.0)?;
        let created = timestamp(&v.1, v.2)?;
        let d = decode_durability(v.3)?;
        let expected = SessionRevision::from_store(read_u64(&v.4)?);
        let next = SessionRevision::from_store(read_u64(&v.5)?);
        let receipt = ArchiveReceipt::from_store(
            ArchiveSession::new(sid, expected),
            SessionHead::from_store(sid, initial, expected, created, d, false),
            SessionHead::from_store(sid, initial, next, created, d, true),
        );
        if expected.checked_next() != Some(next)
            || session_head(c, sid)? != Some(receipt.session())
            || archive_receipt_integrity(receipt)? != v.6
        {
            return Err(SessionStoreError::corrupt());
        }
        Ok(receipt)
    })
    .transpose()
}

/// A checkpoint of another session whose parent or fork source is in session
/// `?1`.
pub(in super::super) const SESSION_FOREIGN_DEPENDENCY: &str = "SELECT 1 FROM checkpoints c WHERE c.session_id<>?1 AND (c.parent_id IN (SELECT checkpoint_id FROM checkpoints WHERE session_id=?1) OR c.fork_checkpoint IN (SELECT checkpoint_id FROM checkpoints WHERE session_id=?1)) LIMIT 1";

/// Session `?1`'s dependent rows, children before parents; its checkpoints
/// and session row go after their released roots are queued. The lines whose
/// head is in the session go with it; a line whose head moved to another
/// session keeps going, and its branches here were read-only predecessors.
pub(in super::super) const SESSION_ROW_DELETES: [&str; 11] = [
    "DELETE FROM line_job_terminals WHERE line_id IN (SELECT line_id FROM lines WHERE head_session=?1)",
    "DELETE FROM lines WHERE head_session=?1",
    "DELETE FROM fork_receipts WHERE source_session=?1",
    "DELETE FROM fork_receipts WHERE target_session=?1",
    "DELETE FROM commit_receipts WHERE session_id=?1",
    "DELETE FROM archive_receipts WHERE session_id=?1",
    "DELETE FROM create_receipts WHERE session_id=?1",
    "DELETE FROM branch_horizons WHERE session_id=?1",
    "DELETE FROM pruned_checkpoints WHERE session_id=?1",
    "DELETE FROM branches WHERE session_id=?1",
    "DELETE FROM session_feed WHERE session_id=?1",
];

pub(in super::super) fn delete(
    b: &mut Backend,
    r: DeleteSession,
) -> Result<DeleteReceipt, SessionStoreError> {
    b.require_open()?;
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    if deletion_exists(&tx, r.session_id())? {
        let receipt = load_delete(&tx, r)?;
        tx.commit().map_err(map_sqlite)?;
        return Ok(receipt);
    }
    let archived = session_head(&tx, r.session_id())?.ok_or(SessionStoreError::NotFound)?;
    if !archived.is_archived() {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::SessionNotArchived,
        ));
    }
    if archived.revision() != r.expected_revision() {
        return Err(SessionStoreError::Conflict);
    }
    let branches = count_where(&tx, "branches", r.session_id())?;
    let checkpoints = count_where(&tx, "checkpoints", r.session_id())?;
    let foreign_dependency: Option<i64> = tx
        .query_row(SESSION_FOREIGN_DEPENDENCY, [id(r.session_id())], |row| {
            row.get(0)
        })
        .optional()
        .map_err(map_sqlite)?;
    if foreign_dependency.is_some() {
        return Err(SessionStoreError::corrupt());
    }

    for statement in SESSION_ROW_DELETES {
        tx.execute(statement, [id(r.session_id())])
            .map_err(map_sqlite)?;
    }
    capture_released_roots(&tx, "session_id=?1", &id(r.session_id()))?;
    tx.execute(
        "DELETE FROM checkpoints WHERE session_id=?1",
        [id(r.session_id())],
    )
    .map_err(map_sqlite)?;
    tx.execute(
        "DELETE FROM sessions WHERE session_id=?1",
        [id(r.session_id())],
    )
    .map_err(map_sqlite)?;

    let receipt = DeleteReceipt::from_store(r, archived, checkpoints);
    let integrity = delete_receipt_integrity(receipt)?;
    tx.execute(
        "INSERT INTO delete_tombstones VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            id(r.session_id()),
            id(archived.initial_branch()),
            u64b(archived.created_at().seconds()),
            i64::from(archived.created_at().nanoseconds()),
            durability_tag(archived.durability())?,
            u64b(archived.revision().get()),
            i64n(checkpoints)?,
            integrity
        ],
    )
    .map_err(map_sqlite)?;
    tx.execute("UPDATE store_meta SET sessions=sessions-1,branches=branches-?1,checkpoints=checkpoints-?2,semantic_mutations=semantic_mutations+1,tombstones=tombstones+1 WHERE id=1", params![i64n(branches)?,i64n(checkpoints)?]).map_err(map_sqlite)?;
    tx.commit().map_err(map_sqlite)?;
    b.gc_pending = true;
    Ok(receipt)
}

pub(in super::super) fn load_delete(
    c: &Connection,
    r: DeleteSession,
) -> Result<DeleteReceipt, SessionStoreError> {
    let row = c.query_row("SELECT initial_branch,created_s,created_ns,durability,archived_revision,reclaimed_checkpoints,receipt_integrity FROM delete_tombstones WHERE session_id=?1", [id(r.session_id())], |z| Ok((z.get::<_,String>(0)?,z.get::<_,Vec<u8>>(1)?,z.get::<_,i64>(2)?,z.get::<_,i64>(3)?,z.get::<_,Vec<u8>>(4)?,z.get::<_,i64>(5)?,z.get::<_,Vec<u8>>(6)?))).optional().map_err(map_sqlite)?.ok_or(SessionStoreError::NotFound)?;
    let revision = SessionRevision::from_store(read_u64(&row.4)?);
    if revision == SessionRevision::INITIAL || revision != r.expected_revision() {
        return Err(SessionStoreError::Conflict);
    }
    if session_head(c, r.session_id())?.is_some() {
        return Err(SessionStoreError::corrupt());
    }
    let archived = SessionHead::from_store(
        r.session_id(),
        parse(&row.0)?,
        revision,
        timestamp(&row.1, row.2)?,
        decode_durability(row.3)?,
        true,
    );
    let receipt = DeleteReceipt::from_store(r, archived, nonnegative(row.5)?);
    if delete_receipt_integrity(receipt)? != row.6 {
        return Err(SessionStoreError::corrupt());
    }
    Ok(receipt)
}

pub(in super::super) fn deletion_exists(
    c: &Connection,
    session_id: SessionId,
) -> Result<bool, SessionStoreError> {
    c.query_row(
        "SELECT 1 FROM delete_tombstones WHERE session_id=?1",
        [id(session_id)],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
    .map_err(map_sqlite)
}

fn count_where(c: &Connection, table: &str, sid: SessionId) -> Result<u64, SessionStoreError> {
    c.query_row(
        &format!("SELECT count(*) FROM {table} WHERE session_id=?1"),
        [id(sid)],
        |r| r.get::<_, i64>(0),
    )
    .map_err(map_sqlite)
    .and_then(nonnegative)
}
