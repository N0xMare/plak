//! Branch history prune.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the sqlite module that shares its vocabulary"
)]

use super::super::*;

/// Checkpoints of branch `?1`/`?2` strictly below revision `?3` that no
/// receipt, fork, other branch or foreign child still names.
pub(in super::super) const PRUNE_CANDIDATES: &str = "SELECT checkpoint_id FROM checkpoints c
     WHERE c.session_id=?1 AND c.branch_id=?2 AND c.revision<?3
     AND NOT EXISTS (SELECT 1 FROM create_receipts r
         WHERE r.session_id=?1 AND r.checkpoint_id=c.checkpoint_id)
     AND NOT EXISTS (SELECT 1 FROM fork_receipts f WHERE f.source_checkpoint=c.checkpoint_id)
     AND NOT EXISTS (SELECT 1 FROM fork_receipts f WHERE f.target_checkpoint=c.checkpoint_id)
     AND NOT EXISTS (SELECT 1 FROM checkpoints o WHERE o.fork_checkpoint=c.checkpoint_id)
     AND NOT EXISTS (SELECT 1 FROM branches b WHERE b.fork_checkpoint=c.checkpoint_id)
     AND NOT EXISTS (SELECT 1 FROM checkpoints o WHERE o.parent_id=c.checkpoint_id
         AND (o.session_id<>?1 OR o.branch_id<>?2))
     ORDER BY checkpoint_id";

#[allow(
    clippy::too_many_lines,
    reason = "the one prune transaction stays linear so its retained set is auditable"
)]
pub(in super::super) fn prune(
    b: &mut Backend,
    r: &PruneBranch,
) -> Result<PruneReceipt, SessionStoreError> {
    b.require_open()?;
    let selector = r.expected().session();
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    let now = sample_clock(b.clock.as_ref(), b.domain)?;
    live_session(&tx, selector.session_id())?;
    let row = branch_row(&tx, selector)?.ok_or(SessionStoreError::NotFound)?;
    require_lease(&row, r.lease(), now)?;
    if row.head != r.expected() {
        return Err(SessionStoreError::Conflict);
    }
    // A predecessor branch of a live line is read-only history: never pruned.
    let (line, generation) =
        lines::branch_line(&tx, selector)?.ok_or_else(SessionStoreError::corrupt)?;
    lines::require_line_head(&tx, selector, line, generation)?;
    let head = checkpoint_row(&tx, row.head.checkpoint().checkpoint_id())?
        .ok_or(SessionStoreError::corrupt())?;
    if head.kind != RuntimeCheckpointKind::Quiescent {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::PruneHeadNotQuiescent,
        ));
    }
    let previous = load_horizon(&tx, selector)?;
    let unchanged = PruneReceipt::from_store(
        row.head,
        previous
            .as_ref()
            .map_or(BranchRevision::INITIAL, |h| h.revision),
        0,
    );
    // The head's parent is the horizon: everything strictly below it that no
    // receipt, fork, other branch or the runtime still names is superseded.
    let (Some(horizon_revision), Some(horizon_checkpoint)) =
        (row.head.revision().get().checked_sub(1), head.parent)
    else {
        tx.commit().map_err(map_sqlite)?;
        return Ok(unchanged);
    };
    let mut statement = tx.prepare(PRUNE_CANDIDATES).map_err(map_sqlite)?;
    let superseded = statement
        .query_map(
            params![
                id(selector.session_id()),
                id(selector.branch_id()),
                u64b(horizon_revision)
            ],
            |z| z.get::<_, String>(0),
        )
        .map_err(map_sqlite)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sqlite)?
        .iter()
        .map(|value| parse::<CheckpointId>(value))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|checkpoint| !r.retain().contains(checkpoint))
        .collect::<Vec<_>>();
    drop(statement);
    let pruned = u64n(superseded.len());
    if pruned == 0 || pruned < r.minimum() {
        tx.commit().map_err(map_sqlite)?;
        return Ok(unchanged);
    }
    tx.execute(
        "DELETE FROM commit_receipts WHERE checkpoint_id IN
         (SELECT checkpoint_id FROM checkpoints WHERE session_id=?1 AND branch_id=?2
          AND revision<=?3)",
        params![
            id(selector.session_id()),
            id(selector.branch_id()),
            u64b(horizon_revision)
        ],
    )
    .map_err(map_sqlite)?;
    for checkpoint in &superseded {
        tx.execute(
            "INSERT INTO pruned_checkpoints VALUES(?1,?2,?3)",
            params![
                id(checkpoint),
                id(selector.session_id()),
                id(selector.branch_id())
            ],
        )
        .map_err(map_sqlite)?;
    }
    let total = |field: fn(&BranchHorizon) -> u64, delta: u64| {
        previous
            .as_ref()
            .map_or(0, field)
            .checked_add(delta)
            .ok_or(SessionStoreError::corrupt())
    };
    // The horizon row moves to its new checkpoint before the old one can go.
    let horizon = BranchHorizon {
        selector,
        revision: BranchRevision::from_store(horizon_revision),
        checkpoint: horizon_checkpoint,
        pruned_checkpoints: total(|h| h.pruned_checkpoints, pruned)?,
    };
    write_horizon(&tx, &horizon)?;
    for checkpoint in &superseded {
        capture_released_roots(&tx, "checkpoint_id=?1", &id(checkpoint))?;
        exactly_one(
            tx.execute(
                "DELETE FROM checkpoints WHERE checkpoint_id=?1",
                [id(checkpoint)],
            )
            .map_err(map_sqlite)?,
        )?;
    }
    #[cfg(test)]
    test_crash_cut("prune-after-enqueue");

    feed::append(
        &tx,
        selector,
        FeedChange::Pruned {
            head: row.head.checkpoint().checkpoint_id(),
            horizon: horizon.revision,
        },
        &[],
    )?;
    tx.execute("UPDATE store_meta SET checkpoints=checkpoints-?1,semantic_mutations=semantic_mutations+1 WHERE id=1", [i64n(pruned)?]).map_err(map_sqlite)?;
    #[cfg(test)]
    test_crash_cut("prune-before-commit");
    tx.commit().map_err(map_sqlite)?;
    #[cfg(test)]
    test_crash_cut("prune-after-commit");
    // Returning free pages is scheduled after the authoritative commit, not
    // awaited on the prune acknowledgment path.
    b.gc_pending = true;
    b.vacuum_pending = true;
    b.wal_pending = true;
    Ok(PruneReceipt::from_store(row.head, horizon.revision, pruned))
}

/// Returns up to `pages` free pages to the file system. The pragma frees one
/// page per result row, so every row must be stepped.
pub(in super::super) fn incremental_vacuum(
    c: &Connection,
    pages: u32,
) -> Result<(), SessionStoreError> {
    let mut statement = c
        .prepare(&format!("PRAGMA incremental_vacuum({pages})"))
        .map_err(map_sqlite)?;
    let mut rows = statement.query([]).map_err(map_sqlite)?;
    while rows.next().map_err(map_sqlite)?.is_some() {}
    Ok(())
}

fn write_horizon(tx: &Transaction<'_>, h: &BranchHorizon) -> Result<(), SessionStoreError> {
    tx.execute(
        "INSERT INTO branch_horizons VALUES(?1,?2,?3,?4,?5,?6)
         ON CONFLICT(session_id,branch_id) DO UPDATE SET horizon_revision=excluded.horizon_revision,
         horizon_checkpoint=excluded.horizon_checkpoint,
         pruned_checkpoints=excluded.pruned_checkpoints,
         integrity=excluded.integrity",
        params![
            id(h.selector.session_id()),
            id(h.selector.branch_id()),
            u64b(h.revision.get()),
            id(h.checkpoint),
            i64n(h.pruned_checkpoints)?,
            horizon_integrity(h)?
        ],
    )
    .map_err(map_sqlite)?;
    Ok(())
}
