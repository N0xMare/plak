//! Checkpoint commit transaction.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the sqlite module that shares its vocabulary"
)]

use super::super::*;
use crate::memory::receipts::commit_matches;

pub(in super::super) fn commit(
    backend: &mut Backend,
    request: &CommitCheckpoint,
) -> Result<CommitReceipt, SessionStoreError> {
    commit_impl(backend, request, false).map(|(receipt, _)| receipt)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in super::super) struct CommitExecution {
    pub(in super::super) transaction_begin_count: u64,
    pub(in super::super) transaction_commit_count: u64,
    pub(in super::super) transaction_elapsed_ns: u64,
    pub(in super::super) commit_call_elapsed_ns: u64,
    pub(in super::super) inserted: InsertEvidence,
}

#[cfg(feature = "perf-instrumentation")]
pub(in super::super) fn commit_observed(
    backend: &mut Backend,
    request: &CommitCheckpoint,
) -> Result<(CommitReceipt, CommitExecution), SessionStoreError> {
    commit_impl(backend, request, true)
}

#[allow(
    clippy::too_many_lines,
    reason = "the single SQLite transaction keeps validate-then-publish atomicity auditable"
)]
fn commit_impl(
    backend: &mut Backend,
    request: &CommitCheckpoint,
    observed: bool,
) -> Result<(CommitReceipt, CommitExecution), SessionStoreError> {
    #[cfg(not(feature = "perf-instrumentation"))]
    let _ = observed;
    backend.require_open()?;
    let reference = bundle_ref(request.checkpoint());
    let candidate = candidate(request.checkpoint())?;
    #[cfg(feature = "perf-instrumentation")]
    let transaction_started = observed.then(Instant::now);
    let tx = backend
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, backend.domain)?;
    let now = sample_clock(backend.clock.as_ref(), backend.domain)?;
    if let Some(old) = load_commit_receipt(&tx, reference.checkpoint_id())? {
        let out = if commit_matches(
            &old,
            request,
            reference,
            candidate.checkpoint.checkpoint_commit_identity(),
        ) {
            Ok(old.receipt())
        } else {
            Err(SessionStoreError::IdentityConflict)
        };
        #[cfg(feature = "perf-instrumentation")]
        let commit_started = observed.then(Instant::now);
        tx.commit().map_err(map_sqlite)?;
        #[cfg(feature = "perf-instrumentation")]
        let execution = CommitExecution {
            transaction_begin_count: 1,
            transaction_commit_count: 1,
            transaction_elapsed_ns: transaction_started.map_or(0, elapsed_ns),
            commit_call_elapsed_ns: commit_started.map_or(0, elapsed_ns),
            inserted: InsertEvidence::default(),
        };
        #[cfg(not(feature = "perf-instrumentation"))]
        let execution = CommitExecution::default();
        return out.map(|receipt| (receipt, execution));
    }
    if commit_history_pruned(&tx, reference.checkpoint_id())? {
        return Err(SessionStoreError::Pruned);
    }
    let session = live_session(&tx, request.expected().session().session_id())?;
    let row = branch_row(&tx, request.expected().session())?.ok_or(SessionStoreError::NotFound)?;
    require_lease(&row, request.lease(), now)?;
    if row.head != request.expected() {
        return Err(SessionStoreError::Conflict);
    }
    let expected = checkpoint_row(&tx, request.expected().checkpoint().checkpoint_id())?
        .ok_or(SessionStoreError::corrupt())?;
    let next = request
        .expected()
        .revision()
        .checked_next()
        .ok_or_else(|| {
            SessionStoreError::capacity(
                StoreCapacityBound::BranchRevision,
                request.expected().revision().get(),
                0,
            )
        })?;
    let topology = candidate.checkpoint.manifest().topology();
    if candidate.base != Some(expected.reference.root())
        || expected.reference != request.expected().checkpoint()
        || topology.session_id != request.expected().session().session_id()
        || topology.branch_id != request.expected().session().branch_id()
        || topology.checkpoint_id != reference.checkpoint_id()
        || topology.parent_checkpoint_id != Some(expected.reference.checkpoint_id())
        || topology.branch_revision != next.get()
        || topology.fork != row.lineage
        || topology.lease_fence != request.lease().fence().get()
        || candidate.checkpoint.manifest().durability() != session.durability()
    {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::CheckpointCommitTopology,
        ));
    }
    // The head fence: only the line's current head commits.
    lines::require_branch_is_head(
        &tx,
        request.expected().session(),
        topology.line,
        topology.line_generation,
        StoreInvalidReason::CheckpointCommitTopology,
    )?;
    lines::consume_terminals(
        &tx,
        topology.line,
        topology.line_generation,
        request.consumed_line_terminals(),
    )?;
    let inserted = insert_candidate(&tx, &candidate, backend.limits, request.admission())?;
    let head = BranchHead::from_store(request.expected().session(), next, reference);
    tx.execute(
        "UPDATE branches SET revision=?3,checkpoint_id=?4 WHERE session_id=?1 AND branch_id=?2",
        params![
            id(head.session().session_id()),
            id(head.session().branch_id()),
            u64b(next.get()),
            id(reference.checkpoint_id())
        ],
    )
    .map_err(map_sqlite)?;
    let feed_seq = feed::append(
        &tx,
        request.expected().session(),
        FeedChange::Advanced {
            parent: request.expected().checkpoint().checkpoint_id(),
            checkpoint: reference.checkpoint_id(),
            revision: next,
            head: feed::head_kind(candidate.checkpoint.kind()),
        },
        request.feed_facts(),
    )?;
    let lease = request.lease();
    let receipt = CommitReceipt::from_store(reference, request.expected(), head, feed_seq);
    let committed = CommittedCheckpoint::from_store(
        CommitOperation::from_store(
            lease.clone(),
            request.expected(),
            reference,
            candidate.checkpoint.checkpoint_commit_identity(),
        ),
        receipt,
    );
    let receipt_integrity = commit_receipt_integrity(&committed)?;
    exactly_one(
        tx.execute(
            "INSERT INTO commit_receipts VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                id(reference.checkpoint_id()),
                id(lease.session().session_id()),
                id(lease.session().branch_id()),
                id(lease.owner()),
                u64b(lease.fence().get()),
                lease_time_bytes(lease.expires_at()),
                i64::from(lease.expires_at().nanoseconds()),
                u64b(lease.lease_seconds().get()),
                u64b(request.expected().revision().get()),
                id(request.expected().checkpoint().checkpoint_id()),
                u64b(next.get()),
                receipt_integrity,
                i64n(feed_seq.get())?
            ],
        )
        .map_err(map_sqlite)?,
    )?;
    bump(&tx)?;
    #[cfg(test)]
    test_crash_cut("before-commit");
    #[cfg(test)]
    test_crash_signal("async-in-commit");
    #[cfg(feature = "perf-instrumentation")]
    let commit_started = observed.then(Instant::now);
    tx.commit().map_err(map_sqlite)?;
    #[cfg(test)]
    test_crash_cut("async-in-commit");
    backend.checkpoints.seed(
        request.expected().session(),
        reference,
        request.checkpoint(),
        Some(request.expected().checkpoint().checkpoint_id()),
    );
    #[cfg(feature = "perf-instrumentation")]
    let execution = CommitExecution {
        transaction_begin_count: 1,
        transaction_commit_count: 1,
        transaction_elapsed_ns: transaction_started.map_or(0, elapsed_ns),
        commit_call_elapsed_ns: commit_started.map_or(0, elapsed_ns),
        inserted,
    };
    #[cfg(not(feature = "perf-instrumentation"))]
    let execution = CommitExecution {
        inserted,
        ..CommitExecution::default()
    };
    #[cfg(test)]
    test_crash_cut("post-commit-pre-reply");
    Ok((receipt, execution))
}

pub(in super::super) fn recover_commit(
    c: &Connection,
    idv: CheckpointId,
) -> Result<CommittedCheckpoint, SessionStoreError> {
    match load_commit_receipt(c, idv)? {
        Some(committed) => Ok(committed),
        None if commit_history_pruned(c, idv)? => Err(SessionStoreError::Pruned),
        None => Err(SessionStoreError::NotFound),
    }
}

fn load_commit_receipt(
    c: &Connection,
    idv: CheckpointId,
) -> Result<Option<CommittedCheckpoint>, SessionStoreError> {
    c.query_row(
        "SELECT session_id,branch_id,owner,fence,expires_time,expires_ns,duration,
        expected_revision,expected_checkpoint,head_revision,receipt_integrity,feed_seq
        FROM commit_receipts WHERE checkpoint_id=?1",
        [id(idv)],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Vec<u8>>(3)?,
                r.get::<_, Vec<u8>>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, Vec<u8>>(6)?,
                r.get::<_, Vec<u8>>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, Vec<u8>>(9)?,
                r.get::<_, Vec<u8>>(10)?,
                r.get::<_, i64>(11)?,
            ))
        },
    )
    .optional()
    .map_err(map_sqlite)?
    .map(|v| {
        let s = SessionRef::new(parse(&v.0)?, parse(&v.1)?);
        let l = BranchLease::from_store(
            s,
            parse(&v.2)?,
            nz(&v.3)?,
            lease_time(&v.4, v.5)?,
            nz(&v.6)?,
        );
        let old = checkpoint_row(c, parse(&v.8)?)?.ok_or(SessionStoreError::corrupt())?;
        let new = checkpoint_row(c, idv)?.ok_or(SessionStoreError::corrupt())?;
        let previous_revision = BranchRevision::from_store(read_u64(&v.7)?);
        let head_revision = BranchRevision::from_store(read_u64(&v.9)?);
        if previous_revision.checked_next() != Some(head_revision)
            || old.selector != s
            || new.selector != s
            || old.revision != previous_revision
            || new.revision != head_revision
            || new.parent != Some(old.reference.checkpoint_id())
            || new.lineage != old.lineage
            || new.fence != l.fence().get()
            || new.durability != old.durability
        {
            return Err(SessionStoreError::corrupt());
        }
        let previous = BranchHead::from_store(s, previous_revision, old.reference);
        let head = BranchHead::from_store(s, head_revision, new.reference);
        let committed = CommittedCheckpoint::from_store(
            CommitOperation::from_store(l, previous, new.reference, new.commit),
            CommitReceipt::from_store(new.reference, previous, head, feed::receipt_seq(v.11)?),
        );
        if commit_receipt_integrity(&committed)? != v.10 {
            return Err(SessionStoreError::corrupt());
        }
        Ok(committed)
    })
    .transpose()
}
