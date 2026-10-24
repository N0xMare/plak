//! Fork transaction and checkpoint reads.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the sqlite module that shares its vocabulary"
)]

use super::super::*;

// Keeping the full transaction linear makes its validate-then-publish boundary auditable.
#[allow(
    clippy::too_many_lines,
    reason = "the single transaction remains linear so validation and publication are auditable"
)]
pub(in super::super) fn fork(
    b: &mut Backend,
    r: &ForkBranch,
) -> Result<ForkReceipt, SessionStoreError> {
    b.require_open()?;
    crate::memory::validate_fork_semantic_request(r)?;
    let reference = bundle_ref(r.target());
    let x = candidate(r.target())?;
    let target = SessionRef::new(r.source().session().session_id(), r.target_branch());
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    let now = sample_clock(b.clock.as_ref(), b.domain)?;
    if let Some(old) = load_fork(&tx, r.workflow_id())? {
        let out = if fork_matches(
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
        return out;
    }
    let expires = expiry(now, r.lease_seconds())?;
    let session = live_session(&tx, target.session_id())?;
    if session.revision() != r.expected_session_revision() {
        return Err(SessionStoreError::Conflict);
    }
    if branch_row(&tx, target)?.is_some() {
        return Err(SessionStoreError::IdentityConflict);
    }
    let count: u64 = tx
        .query_row(
            "SELECT count(*) FROM branches WHERE session_id=?1",
            [id(target.session_id())],
            |z| z.get::<_, i64>(0),
        )
        .map_err(map_sqlite)
        .and_then(nonnegative)?;
    enforce(
        StoreCapacityBound::BranchesPerSession,
        count + 1,
        b.limits.max_branches_per_session().get(),
    )?;
    let source = branch_row(&tx, r.source().session())?.ok_or(SessionStoreError::NotFound)?;
    require_lease(&source, r.source_lease(), now)?;
    if source.head != r.source() {
        return Err(SessionStoreError::Conflict);
    }
    // The source head is usually retained from the attach that preceded this
    // fork; its row, probed in this transaction, binds the exact head.
    let source_id = source.head.checkpoint().checkpoint_id();
    let source_row = checkpoint_row(&tx, source_id)?.ok_or_else(SessionStoreError::corrupt)?;
    let source_cp = b.checkpoints.load(&tx, source_id, &source_row)?;
    let source_checkpoint = source_cp.checkpoint();
    let t = x.checkpoint.manifest().topology();
    let lineage = ForkLineage {
        session_id: r.source().session().session_id(),
        branch_id: r.source().session().branch_id(),
        checkpoint_id: r.source().checkpoint().checkpoint_id(),
        branch_revision: r.source().revision().get(),
    };
    if source_checkpoint.kind() != RuntimeCheckpointKind::Quiescent
        || x.checkpoint.kind() != RuntimeCheckpointKind::Quiescent
        || x.base != Some(source_cp.reference().root())
        || t.session_id != target.session_id()
        || t.branch_id != target.branch_id()
        || t.checkpoint_id != reference.checkpoint_id()
        || t.parent_checkpoint_id != Some(source_cp.reference().checkpoint_id())
        || t.branch_revision != 0
        || t.fork != Some(lineage)
        || t.lease_fence != 1
        || x.checkpoint.manifest().durability() != session.durability()
    {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::ForkCheckpointSemantics,
        ));
    }
    crate::memory::validate_branch_manifest_transition(
        source_checkpoint,
        &x.checkpoint,
        r.transition(),
    )?;
    let revision = r
        .expected_session_revision()
        .checked_next()
        .ok_or_else(|| {
            SessionStoreError::capacity(
                StoreCapacityBound::SessionRevision,
                r.expected_session_revision().get(),
                0,
            )
        })?;
    let lease_fence = NonZeroU64::MIN;
    let lease = BranchLease::from_store(target, r.owner(), lease_fence, expires, r.lease_seconds());
    let head = BranchHead::from_store(target, BranchRevision::INITIAL, reference);
    insert_candidate(&tx, &x, b.limits, r.admission())?;
    tx.execute(
        "UPDATE sessions SET revision=?2 WHERE session_id=?1",
        params![id(target.session_id()), u64b(revision.get())],
    )
    .map_err(map_sqlite)?;
    insert_branch(&tx, head, Some(r.source()), 1, Some(&lease))?;
    let feed_seq = feed::append(
        &tx,
        target,
        FeedChange::Branched {
            source: FeedLocator {
                session_id: r.source().session().session_id(),
                branch_id: r.source().session().branch_id(),
                checkpoint_id: r.source().checkpoint().checkpoint_id(),
            },
            target: reference.checkpoint_id(),
            revision,
            transition: match r.transition() {
                BranchTransitionMode::OrdinaryFork => FeedBranchTransition::Fork,
                BranchTransitionMode::Rebind => FeedBranchTransition::Rebind,
            },
        },
        r.feed_facts(),
    )?;
    let next = SessionHead::from_store(
        session.session_id(),
        session.initial_branch(),
        revision,
        session.created_at(),
        session.durability(),
        false,
    );
    let receipt = ForkReceipt::from_store(
        ForkOperation::from_store(
            r.workflow_id(),
            r.semantic_request(),
            r.expected_session_revision(),
            r.source(),
            target,
            r.transition(),
            reference,
            x.checkpoint.checkpoint_commit_identity(),
        ),
        session,
        next,
        r.source(),
        head,
        lease,
        feed_seq,
    );
    let receipt_integrity = fork_receipt_integrity(&receipt)?;
    exactly_one(
        tx.execute(
            "INSERT INTO fork_receipts VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,
        ?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25)",
            params![
                workflow_id_bytes(r.workflow_id()),
                id(target.session_id()),
                id(target.branch_id()),
                id(r.source().session().session_id()),
                id(r.source().session().branch_id()),
                u64b(r.expected_session_revision().get()),
                u64b(r.source().revision().get()),
                i64::from(r.semantic_request().version()),
                r.semantic_request().bytes().to_vec(),
                id(r.source().checkpoint().checkpoint_id()),
                transition_tag(r.transition()),
                id(reference.checkpoint_id()),
                id(r.owner()),
                u64b(r.lease_seconds().get()),
                u64b(1),
                lease_time_bytes(expires),
                i64::from(expires.nanoseconds()),
                id(session.initial_branch()),
                u64b(session.created_at().seconds()),
                i64::from(session.created_at().nanoseconds()),
                durability_tag(session.durability())?,
                u64b(session.revision().get()),
                u64b(next.revision().get()),
                receipt_integrity,
                i64n(feed_seq.get())?
            ],
        )
        .map_err(map_sqlite)?,
    )?;
    meta(&tx, 0, 1)?;
    super::lease::commit_claimed(tx, &mut b.holders, target, lease_fence)?;
    b.checkpoints.seed_fork(target, reference, r.target());
    Ok(receipt)
}

pub(in super::super) fn recover_fork(
    b: &Backend,
    r: &RecoverFork,
) -> Result<ForkReceipt, SessionStoreError> {
    let receipt = load_fork(&b.connection, r.workflow_id())?.ok_or(SessionStoreError::NotFound)?;
    if !recover_fork_matches(&receipt, r) {
        return Err(SessionStoreError::IdentityConflict);
    }
    let source =
        branch_row(&b.connection, r.source().session())?.ok_or(SessionStoreError::NotFound)?;
    require_lease(
        &source,
        r.source_lease(),
        sample_clock(b.clock.as_ref(), b.domain)?,
    )?;
    Ok(receipt)
}

#[allow(
    clippy::too_many_lines,
    reason = "linear receipt decode keeps historical source/target validation auditable"
)]
fn load_fork(
    c: &Connection,
    workflow_id: SessionWorkflowId,
) -> Result<Option<ForkReceipt>, SessionStoreError> {
    c.query_row(
        "SELECT target_session,target_branch,source_session,source_branch,
        expected_session_revision,source_revision,semantic_request_version,semantic_request,
        source_checkpoint,transition,target_checkpoint,
        target_owner,target_duration,target_fence,target_expires_time,target_expires_ns,
        initial_branch,created_s,created_ns,durability,previous_session_revision,session_revision,
        receipt_integrity,feed_seq FROM fork_receipts WHERE workflow_id=?1",
        [workflow_id_bytes(workflow_id)],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Vec<u8>>(4)?,
                r.get::<_, Vec<u8>>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, Vec<u8>>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, i64>(9)?,
                r.get::<_, String>(10)?,
                r.get::<_, String>(11)?,
                r.get::<_, Vec<u8>>(12)?,
                r.get::<_, Vec<u8>>(13)?,
                r.get::<_, Vec<u8>>(14)?,
                r.get::<_, i64>(15)?,
                r.get::<_, String>(16)?,
                r.get::<_, Vec<u8>>(17)?,
                r.get::<_, i64>(18)?,
                r.get::<_, i64>(19)?,
                r.get::<_, Vec<u8>>(20)?,
                r.get::<_, Vec<u8>>(21)?,
                r.get::<_, Vec<u8>>(22)?,
                r.get::<_, i64>(23)?,
            ))
        },
    )
    .optional()
    .map_err(map_sqlite)?
    .map(|v| {
        let target = SessionRef::new(parse(&v.0)?, parse(&v.1)?);
        let source_ref = SessionRef::new(parse(&v.2)?, parse(&v.3)?);
        let expected_session_revision = SessionRevision::from_store(read_u64(&v.4)?);
        let source_revision = BranchRevision::from_store(read_u64(&v.5)?);
        let semantic_request = decode_digest(DigestDomain::RequestPlan, v.6, v.7)?;
        let source_checkpoint = load_checkpoint(c, parse(&v.8)?)?;
        let transition = decode_transition(v.9)?;
        let target_checkpoint = load_checkpoint(c, parse(&v.10)?)?;
        let owner: RunId = parse(&v.11)?;
        let duration = nz(&v.12)?;
        let lease = BranchLease::from_store(
            target,
            owner,
            nz(&v.13)?,
            lease_time(&v.14, v.15)?,
            duration,
        );
        let initial = parse(&v.16)?;
        let created = timestamp(&v.17, v.18)?;
        let durability = decode_durability(v.19)?;
        let previous_revision = SessionRevision::from_store(read_u64(&v.20)?);
        let session_revision = SessionRevision::from_store(read_u64(&v.21)?);
        let source =
            BranchHead::from_store(source_ref, source_revision, source_checkpoint.reference());
        let previous = SessionHead::from_store(
            target.session_id(),
            initial,
            previous_revision,
            created,
            durability,
            false,
        );
        let session = SessionHead::from_store(
            target.session_id(),
            initial,
            session_revision,
            created,
            durability,
            false,
        );
        let head = BranchHead::from_store(
            target,
            BranchRevision::INITIAL,
            target_checkpoint.reference(),
        );
        let receipt = ForkReceipt::from_store(
            ForkOperation::from_store(
                workflow_id,
                semantic_request,
                expected_session_revision,
                source,
                target,
                transition,
                target_checkpoint.reference(),
                target_checkpoint.checkpoint().checkpoint_commit_identity(),
            ),
            previous,
            session,
            source,
            head,
            lease,
            feed::receipt_seq(v.23)?,
        );
        let persisted_session =
            session_head(c, target.session_id())?.ok_or(SessionStoreError::corrupt())?;
        if persisted_session.initial_branch() != initial
            || persisted_session.created_at() != created
            || persisted_session.durability() != durability
            || fork_receipt_integrity(&receipt)? != v.22
        {
            return Err(SessionStoreError::corrupt());
        }
        crate::memory::validate_materialized_fork_receipt(
            &receipt,
            &source_checkpoint,
            &target_checkpoint,
        )?;
        Ok(receipt)
    })
    .transpose()
}

fn fork_matches(
    v: &ForkReceipt,
    r: &ForkBranch,
    x: CheckpointObjectRef,
    checkpoint_commit_identity: Digest,
) -> bool {
    let o = v.operation();
    o.workflow_id() == r.workflow_id()
        && o.semantic_request() == r.semantic_request()
        && o.expected_session_revision() == r.expected_session_revision()
        && o.source() == r.source()
        && o.target().branch_id() == r.target_branch()
        && o.transition() == r.transition()
        && o.checkpoint() == x
        && o.checkpoint_commit_identity() == checkpoint_commit_identity
        && r.target().base_root() == Some(o.source().checkpoint().root())
}

fn recover_fork_matches(v: &ForkReceipt, r: &RecoverFork) -> bool {
    let o = v.operation();
    o.workflow_id() == r.workflow_id()
        && o.source() == r.source()
        && o.target().branch_id() == r.target_branch()
        && o.transition() == r.transition()
        && o.semantic_request() == r.semantic_request()
}
