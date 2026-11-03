//! Create-session transaction.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the sqlite module that shares its vocabulary"
)]

use super::super::*;
use crate::memory::receipts::create_matches;

#[allow(
    clippy::too_many_lines,
    reason = "the single create transaction stays linear so validation and publication are auditable"
)]
pub(in super::super) fn create(
    b: &mut Backend,
    r: &CreateSession,
) -> Result<CreateReceipt, SessionStoreError> {
    b.require_open()?;
    let reference = bundle_ref(r.initial());
    let x = candidate(r.initial())?;
    validate_genesis(&x, r)?;
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    let expires = expiry(sample_clock(b.clock.as_ref(), b.domain)?, r.lease_seconds())?;
    if let Some(old) = load_create(&tx, r.session().session_id())? {
        let result = if create_matches(
            &old,
            r,
            reference,
            x.checkpoint.checkpoint_commit_identity(),
        ) {
            Ok(old)
        } else {
            Err(SessionStoreError::IdentityConflict)
        };
        tx.commit().map_err(map_sqlite)?;
        return result;
    }
    require_unused_session_identity(&tx, r.session().session_id())?;
    let stats = read_stats(&tx)?;
    enforce(
        StoreCapacityBound::Sessions,
        stats.sessions + 1,
        b.limits.max_sessions().get(),
    )?;
    insert_candidate(&tx, &x, b.limits, r.admission())?;
    let d = x.checkpoint.manifest().durability();
    let lease_fence = NonZeroU64::MIN;
    let lease = BranchLease::from_store(
        r.session(),
        r.owner(),
        lease_fence,
        expires,
        r.lease_seconds(),
    );
    let head = BranchHead::from_store(r.session(), BranchRevision::INITIAL, x.reference);
    let session = SessionHead::from_store(
        r.session().session_id(),
        r.session().branch_id(),
        SessionRevision::INITIAL,
        r.created_at(),
        d,
        false,
    );
    tx.execute(
        "INSERT INTO sessions VALUES(?1,?2,?3,?4,?5,?6,0,0,1,0)",
        params![
            id(session.session_id()),
            id(session.initial_branch()),
            u64b(0),
            u64b(session.created_at().seconds()),
            i64::from(session.created_at().nanoseconds()),
            durability_tag(d)?
        ],
    )
    .map_err(map_sqlite)?;
    let topology = x.checkpoint.manifest().topology();
    insert_branch(
        &tx,
        head,
        None,
        1,
        Some(&lease),
        (topology.line, topology.line_generation),
    )?;
    lines::apply_step(
        &tx,
        r.session(),
        topology.line,
        topology.line_generation,
        None,
        r.line_origin(),
        StoreInvalidReason::GenesisCheckpointTopology,
    )?;
    let feed_seq = feed::append(
        &tx,
        r.session(),
        FeedChange::Created {
            genesis: x.reference.checkpoint_id(),
            revision: BranchRevision::INITIAL,
            line: topology.line,
            generation: topology.line_generation,
        },
        r.feed_facts(),
    )?;
    let receipt = CreateReceipt::from_store(
        CreateOperation::from_store(
            r.session(),
            r.created_at(),
            r.owner(),
            r.lease_seconds(),
            reference,
            x.checkpoint.checkpoint_commit_identity(),
        ),
        session,
        head,
        lease,
        feed_seq,
    );
    let receipt_integrity = create_receipt_integrity(&receipt)?;
    exactly_one(
        tx.execute(
            "INSERT INTO create_receipts VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                id(r.session().session_id()),
                id(r.session().branch_id()),
                u64b(r.created_at().seconds()),
                i64::from(r.created_at().nanoseconds()),
                durability_tag(d)?,
                id(r.owner()),
                u64b(r.lease_seconds().get()),
                id(reference.checkpoint_id()),
                u64b(1),
                lease_time_bytes(expires),
                i64::from(expires.nanoseconds()),
                receipt_integrity,
                i64n(feed_seq.get())?
            ],
        )
        .map_err(map_sqlite)?,
    )?;
    meta(&tx, 1, 1)?;
    super::lease::commit_claimed(tx, &mut b.holders, r.session(), lease_fence)?;
    Ok(receipt)
}

fn validate_genesis(x: &Candidate, r: &CreateSession) -> Result<(), SessionStoreError> {
    let m = x.checkpoint.manifest();
    let t = m.topology();
    if x.checkpoint.kind() != RuntimeCheckpointKind::Quiescent
        || x.base.is_some()
        || t.session_id != r.session().session_id()
        || t.branch_id != r.session().branch_id()
        || t.checkpoint_id != x.reference.checkpoint_id()
        || t.parent_checkpoint_id.is_some()
        || t.branch_revision != 0
        || t.fork.is_some()
        || t.lease_fence != 1
        || matches!(m.durability(), RuntimeDurability::Disabled)
    {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::GenesisCheckpointTopology,
        ));
    }
    Ok(())
}

pub(in super::super) fn recover_create(
    c: &Connection,
    r: &CreateSession,
) -> Result<CreateReceipt, SessionStoreError> {
    let reference = bundle_ref(r.initial());
    let checkpoint_commit_identity = r
        .initial()
        .validated_checkpoint()
        .map_err(bundle_error)?
        .checkpoint_commit_identity();
    match load_create(c, r.session().session_id())? {
        Some(v) if create_matches(&v, r, reference, checkpoint_commit_identity) => Ok(v),
        Some(_) => Err(SessionStoreError::IdentityConflict),
        None => Err(SessionStoreError::NotFound),
    }
}

fn load_create(c: &Connection, sid: SessionId) -> Result<Option<CreateReceipt>, SessionStoreError> {
    c.query_row(
        "SELECT branch_id,created_s,created_ns,durability,owner,duration,checkpoint_id,
        fence,expires_time,expires_ns,receipt_integrity,feed_seq FROM create_receipts WHERE session_id=?1",
        [id(sid)],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Vec<u8>>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, Vec<u8>>(7)?,
                r.get::<_, Vec<u8>>(8)?,
                r.get::<_, i64>(9)?,
                r.get::<_, Vec<u8>>(10)?,
                r.get::<_, i64>(11)?,
            ))
        },
    )
    .optional()
    .map_err(map_sqlite)?
    .map(|v| {
        let selector = SessionRef::new(sid, parse(&v.0)?);
        let created = timestamp(&v.1, v.2)?;
        let d = decode_durability(v.3)?;
        let owner = parse(&v.4)?;
        let duration = nz(&v.5)?;
        let row = checkpoint_row(c, parse(&v.6)?)?.ok_or(SessionStoreError::corrupt())?;
        let lease =
            BranchLease::from_store(selector, owner, nz(&v.7)?, lease_time(&v.8, v.9)?, duration);
        let persisted_session = session_head(c, sid)?.ok_or(SessionStoreError::corrupt())?;
        let persisted_branch = branch_row(c, selector)?.ok_or(SessionStoreError::corrupt())?;
        if row.selector != selector
            || row.revision != BranchRevision::INITIAL
            || row.parent.is_some()
            || row.lineage.is_some()
            || row.fence != lease.fence().get()
            || row.kind != RuntimeCheckpointKind::Quiescent
            || row.durability != d
            || persisted_session.initial_branch() != selector.branch_id()
            || persisted_session.created_at() != created
            || persisted_session.durability() != d
            || persisted_branch.lineage.is_some()
        {
            return Err(SessionStoreError::corrupt());
        }
        let session = SessionHead::from_store(
            sid,
            selector.branch_id(),
            SessionRevision::INITIAL,
            created,
            d,
            false,
        );
        let head = BranchHead::from_store(selector, BranchRevision::INITIAL, row.reference);
        let receipt = CreateReceipt::from_store(
            CreateOperation::from_store(
                selector,
                created,
                owner,
                duration,
                row.reference,
                row.commit,
            ),
            session,
            head,
            lease.clone(),
            feed::receipt_seq(v.11)?,
        );
        if create_receipt_integrity(&receipt)? != v.10 {
            return Err(SessionStoreError::corrupt());
        }
        Ok(receipt)
    })
    .transpose()
}
